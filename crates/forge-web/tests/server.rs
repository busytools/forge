//! The server: it binds what the config says, serves the home, and binds
//! nothing at all when it is turned off.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;

use forge_primitives::McpServerStatus;
use forge_primitives::{
    CurrentModel, EffortLevel, MonitorRecord, MonitorStatus, PermissionMode, SessionSlot, WebConfig,
};
use forge_server::SessionUpdate;
use forge_server::surface::inspector::{ContextUsage, McpServers, ProcessEntry, ProcessSnapshot};
use forge_server::surface::{PendingKind, ViewSurface};
use forge_server::testing::{Fleet, ViewFacts};
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
        .add_task("Busytools", "forge", "Sweep the corpus for banned dashes", "lead", None)
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

    fleet.emit(forge_server::SessionUpdate::CatalogLoaded);

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

    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: forge_primitives::SessionSlot::lead("Busytools", "forge"),
        msg: finished_turn(),
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

/// A turn that finished cleanly, which is the frame that arms the diamond.
fn finished_turn() -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1,
        "duration_api_ms": 1,
        "is_error": false,
        "num_turns": 1,
        "session_id": "s",
    }))
    .expect("a finished turn")
}

/// The state class of the row whose name links to `href`, read off the page
/// the home served. Rows are drawn in document order and each names its own
/// link, so the row opening nearest before it is that link's row.
fn row_class(page: &str, href: &str) -> String {
    let link = format!("href=\"{href}\"");
    let at = page.find(&link).unwrap_or_else(|| panic!("no row links to {href}: {page}"));
    let opening = "<div class=\"row ";
    let start = page[..at].rfind(opening).expect("a row's link sits inside its row");
    let class = &page[start + opening.len()..];
    class[..class.find('"').expect("the class is quoted")].to_owned()
}

/// Serving a session's page is this view showing that session, so its
/// diamond goes. And only its own: the second half is the point, because a
/// clear that wiped the set would leave the home forgetting every other
/// completion the moment the reader opened one.
#[tokio::test]
async fn opening_a_session_page_clears_its_diamond_and_no_other() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    // Two live seats finish a turn with no page showing either of them.
    for project in ["forge", "busymail"] {
        fleet.emit(forge_server::SessionUpdate::ChatAppended {
            key: SessionSlot::lead("Busytools", project),
            msg: finished_turn(),
        });
    }
    // The fold runs in a task of its own. Two yields, for the reason the
    // test above gives: the first wakes it from `recv`, the second runs it
    // to the apply and back to its await.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let (_status, _content_type, before) = get(&config, "/").await;
    assert_eq!(
        row_class(&before, "/session/Busytools/forge/lead"),
        "unseen",
        "precondition: the finished turn armed the row: {before}",
    );
    assert_eq!(
        row_class(&before, "/session/Busytools/busymail/lead"),
        "unseen",
        "precondition: and the other seat's with it: {before}",
    );

    let (status, _content_type, _page) = get(&config, "/session/Busytools/forge/lead").await;
    assert_eq!(status, reqwest::StatusCode::OK, "the page is served");

    let (_status, _content_type, after) = get(&config, "/").await;
    assert_ne!(
        row_class(&after, "/session/Busytools/forge/lead"),
        "unseen",
        "the seat whose page was served has been shown, so its diamond is gone: {after}",
    );
    assert_eq!(
        row_class(&after, "/session/Busytools/busymail/lead"),
        "unseen",
        "and the seat that was not shown keeps its own: {after}",
    );
}

/// A page that is open holds its seat: a turn finishing there is a turn the
/// reader watched, so it arms nothing, and the seat beside it still earns its
/// own diamond.
#[tokio::test]
async fn an_open_page_holds_its_seat() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    // The seat's own stream, held open for the rest of the test: this is the
    // page showing it, and it stays open until the drop at the end.
    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;

    for project in ["forge", "busymail"] {
        fleet.emit(forge_server::SessionUpdate::ChatAppended {
            key: SessionSlot::lead("Busytools", project),
            msg: finished_turn(),
        });
    }
    // The fold runs in a task of its own. Two yields, for the reason the
    // diamond's own test above gives.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let (_status, _content_type, page) = get(&config, "/").await;
    assert_ne!(
        row_class(&page, "/session/Busytools/forge/lead"),
        "unseen",
        "the page open on this seat has shown its turn: {page}",
    );
    assert_eq!(
        row_class(&page, "/session/Busytools/busymail/lead"),
        "unseen",
        "and the seat nobody is showing still earns its own: {page}",
    );

    // The page closes here, which is what lets the seat go. Detaching over
    // the network is not asserted: racing the server's teardown would be a
    // flake, so `the_handler_holds_the_seat_for_its_connection` in
    // `stream.rs` carries that half.
    drop(stream);
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

/// The count sits in its own element, because the sheet colours it apart
/// from the branch beside it: `.row .where .files` draws the count muted
/// where the branch draws dim, and the two only pair if both are there.
#[tokio::test]
async fn the_work_cell_gives_the_count_its_own_element() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    repo_with(dir.path().join("forge").as_path(), 7);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/").await;
    let (_status, _content_type, sheet) = get(&config, "/web.css").await;

    assert!(
        page.contains(
            "<span class=\"where\">main \u{b7} <span class=\"files\">7 files</span></span>"
        ),
        "the branch and the count are elements of their own: {page}",
    );
    assert!(
        sheet.contains(".row .where .files"),
        "and the rule that colours the count is still on that element: {sheet}",
    );
}

/// An artifact is a link only when a browser can follow it. A row whose
/// artifact reads `PR #1210` used to render an anchor to a relative URL of
/// that name, which goes nowhere, and the two cases looked identical on the
/// page while only one of them worked.
#[tokio::test]
async fn the_artifact_links_only_when_a_browser_can_follow_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.add_worker("Busytools", "forge", "em-dash-sweep").expect("forge is declared");
    fleet
        .add_task(
            "Busytools",
            "forge",
            "Land the retry fix",
            "lead",
            Some("https://github.com/busytools/forge/pull/1223"),
        )
        .expect("forge is declared");
    fleet
        .add_task("Busytools", "forge", "Sweep the corpus", "em-dash-sweep", Some("PR #1210"))
        .expect("forge is declared");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/").await;

    assert!(
        page.contains(
            "<a href=\"https://github.com/busytools/forge/pull/1223\" target=\"_blank\" \
             rel=\"noreferrer\">PR 1223</a>"
        ),
        "a URL artifact is the link it names: {page}",
    );
    assert!(
        page.contains(">PR #1210</span>"),
        "and one named by number draws its text alone: {page}",
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
        page.contains("<details class=\"sec\" open data-k=\"sec-git\">"),
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

/// The section's summary counts what has moved in the working tree, which
/// is the same movement the project's row on the home counts, and an
/// untracked file is part of it. The list below the summary is the smaller
/// set a diff can show, and that difference is the two questions rather
/// than two answers to one.
#[tokio::test]
async fn the_git_section_summary_counts_the_movement_the_row_counts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    let repo = dir.path().join("forge");
    repo_with(repo.as_path(), 3);
    // Untracked, so it is in the working tree's count and in nothing the
    // diff below can list.
    std::fs::write(repo.join("untracked.txt"), "new\n").expect("write");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("<span class=\"c2\">main \u{b7} 4 files</span>"),
        "the summary counts the untracked file with the rest: {page}",
    );
    assert_eq!(
        page.matches("<div class=\"file\">").count(),
        3,
        "and the body still lists the three a diff has: {page}",
    );

    let (_status, _content_type, home) = get(&config, "/").await;
    assert!(
        home.contains("<span class=\"files\">4 files</span>"),
        "the row on the home counts the same four, so one fact has one source: {home}",
    );
}

/// A tree whose only movement is untracked files: the summary counts it and
/// the scan cannot list it, so the section says what the count is over rather
/// than opening on nothing. The count is what makes this state need the line.
#[tokio::test]
async fn a_tree_the_scan_cannot_list_says_what_the_list_left_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    let repo = dir.path().join("forge");
    repo_with(repo.as_path(), 1);
    // The one tracked change goes away, leaving the untracked file alone:
    // porcelain 1, and nothing the scan can list.
    git(repo.as_path(), &["checkout", "--", "."]);
    std::fs::write(repo.join("untracked.txt"), "new\n").expect("write");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("<span class=\"c2\">main \u{b7} 1 file</span>"),
        "the summary counts the untracked file: {page}",
    );
    assert!(
        page.contains("<span class=\"k\">not in the diff</span>"),
        "and the body says which file the count is over: {page}",
    );
    assert!(
        page.contains("<details class=\"sec\" open data-k=\"sec-git\">"),
        "so the section opens on it rather than leading the inspector closed: {page}",
    );
}

/// The note under the list is the list's own shortfall, not the tree's. Ten
/// entries move in this tree and the scan can carry seven, so the note says
/// two rather than three: the untracked file its cap could not reach either
/// way is reconciled by the row above it, and counting it here as well would
/// say the same file twice.
#[tokio::test]
async fn the_note_counts_the_lists_shortfall_and_not_the_trees() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    let repo = dir.path().join("forge");
    repo_with(repo.as_path(), 9);
    std::fs::write(repo.join("untracked.txt"), "new\n").expect("write");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("<span class=\"c2\">main \u{b7} 10 files</span>"),
        "the summary counts every entry the tree moved: {page}",
    );
    assert_eq!(page.matches("<div class=\"file\">").count(), 7, "the list holds the cap: {page}");
    assert!(
        page.contains("\u{2026}and 2 more"),
        "the note is the nine the scan found less the seven it carries: {page}",
    );
    assert!(
        !page.contains("\u{2026}and 3 more"),
        "and not the tree's ten less those seven, which would count the untracked file twice: {page}",
    );
    assert!(
        page.contains("<span class=\"k\">not in the diff</span>"),
        "the untracked file is reconciled by its own row instead: {page}",
    );
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
const MOCK: &str = include_str!("../../../docs/book/src/ui/client/web-session.html");

/// The home's mockup, read for the same reason: it is what the home is held
/// against, and the mark and the typefaces are the two things it draws that
/// the page has to match.
const MOCK_HOME: &str = include_str!("../../../docs/book/src/ui/client/web-home.html");

/// The composer's mockup: a specimen read state by state against the built
/// page, so its faces are read as closely as its states.
const MOCK_COMPOSER: &str = include_str!("../../../docs/book/src/ui/client/web-composer.html");

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

/// Every declaration block a stylesheet makes for one selector, whitespace
/// collapsed so the mockup's formatting and the sheet's compare.
///
/// Every one, not the first: a rule that agrees with the mockup at one width
/// and overrides it at another is exactly what a first-block reader misses.
fn blocks_for(css: &str, selector: &str) -> Vec<String> {
    let needle = format!("{selector} {{");
    let mut at = 0;
    let mut out = Vec::new();
    while let Some(found) = css[at..].find(&needle) {
        let start = at + found + needle.len() - 1;
        let rest = &css[start..];
        let close = rest.find('}').unwrap_or_else(|| panic!("{selector} never closes"));
        out.push(rest[1..close].split_whitespace().collect::<Vec<_>>().join(" "));
        at = start + close;
    }
    assert!(!out.is_empty(), "no rule for {selector}");
    out
}

/// The class names inside the element carrying `class="outer"`, in the order
/// they appear there.
fn classes_within(html: &str, outer: &str) -> Vec<String> {
    let marker = format!("class=\"{outer}\"");
    let start = html.find(&marker).unwrap_or_else(|| panic!("no {outer}")).to_owned();
    let rest = &html[start..];
    let end = rest.find("</div>").unwrap_or(rest.len());
    let body = &rest[..end];
    let mut out = Vec::new();
    for chunk in body.split("class=\"").skip(1) {
        if let Some(name) = chunk.split('"').next()
            && name != outer
            && !out.iter().any(|seen| seen == name)
        {
            out.push(name.to_owned());
        }
    }
    out
}

/// The turn body's grid is the mockup's: the declarations, in every block the
/// sheet writes them in, and the class names its own markup carries.
///
/// It rendered as one run-on line with every label fused to its value until
/// the sheet was given these, and nothing in the suite noticed: the fields
/// were all present and every one of them was asserted. A rename takes the
/// grid away again the same way, which is why the names are compared too.
#[tokio::test]
async fn the_turn_body_grid_is_the_mocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let (_status, _content_type, sheet) = get(&config, "/web.css").await;

    // Every block the sheet writes, against every block the mockup writes:
    // a width override is legitimate where the drawing carries it too, and
    // an invented one is not.
    for selector in [".tibody", ".tibody .l, .tibody .n", ".tibody .wide", ".tibody b"] {
        let want = blocks_for(MOCK, selector);
        for block in blocks_for(&sheet, selector) {
            assert!(
                want.contains(&block),
                "the sheet's {selector} draws {block}, which the mockup does not",
            );
        }
    }
}

/// The reader takes every block a selector is written in, which is what
/// makes an override visible to it. It is pinned here because no shipped
/// fixture writes a compared selector twice, so a reader that took the first
/// block would pass everything the sheet has today.
#[test]
fn the_grid_reader_reads_every_block_a_selector_is_written_in() {
    let sheet = ".tibody { display: grid; }\n\
                 @media (max-width: 600px) { .tibody { display: block; } }";

    let blocks = blocks_for(sheet, ".tibody");

    assert_eq!(blocks.len(), 2, "both of them, so an override cannot hide behind the first");
    assert!(
        blocks.contains(&"display: block;".to_owned()),
        "including the one that undoes the grid: {blocks:?}",
    );
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
    assert!(
        !page.contains("class=\"work\""),
        "and no work block for a tail that is not there: twelve pixels of one would \
         push the conversation down: {page}",
    );
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
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: user_frame("u-1", "make the call tree the default"),
    });
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
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
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
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
        fleet.emit(forge_server::SessionUpdate::ChatAppended {
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
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
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
    assert_eq!(
        classes_within(&region, "tibody"),
        classes_within(MOCK, "tibody"),
        "and the body's cells are the mockup's, by name",
    );
    assert!(
        region.contains("93% of input served from cache"),
        "and the share spelled out: {region}",
    );
}

/// A page opened fresh draws a row for each turn it reads. A transcript holds
/// no result frame, so a turn that settled before the page opened has no frame
/// on the wire to draw: what the read can honestly say about it is what the
/// turn's own rows carry.
#[tokio::test]
async fn a_turn_read_from_a_transcript_draws_its_own_row() {
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
                r#"{"type":"user","timestamp":"2026-04-22T04:15:27.000Z","message":{"role":"user","content":"make the call tree the default"}}"#,
                r#"{"type":"assistant","timestamp":"2026-04-22T04:18:08.000Z","message":{"id":"msg_1","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn","usage":{"input_tokens":14,"output_tokens":1711,"cache_read_input_tokens":102194,"cache_creation_input_tokens":7028}}}"#,
                r#"{"type":"user","timestamp":"2026-04-22T04:20:00.000Z","message":{"role":"user","content":"and the one after it"}}"#,
                r#"{"type":"assistant","timestamp":"2026-04-22T04:21:30.000Z","message":{"id":"msg_2","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"Second one done."}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":9,"cache_read_input_tokens":16630,"cache_creation_input_tokens":147}}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        page.matches("class=\"turninfo\"").count(),
        2,
        "each turn draws its own row: {page}",
    );
    // The live row's own name is `turn-live`, so the property is the digit
    // after the prefix rather than the prefix on its own.
    let named_by_place = page
        .split("data-k=\"turn-")
        .skip(1)
        .any(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit()));
    assert!(!named_by_place, "no row is named by its place in the conversation: {page}");
    assert!(
        page.contains("data-k=\"2026-04-22T04:15:27.000Z\""),
        "the first row is named for its own turn, so nothing inserted above it renames it: {page}",
    );
    assert!(
        page.contains("data-k=\"2026-04-22T04:20:00.000Z\""),
        "and the second for its own: {page}",
    );
    assert!(page.contains("2m 41s"), "the first turn's own wall clock: {page}");
    assert!(page.contains("1m 30s"), "and the second's: {page}");
    assert!(page.contains("14\u{2191}"), "the counts its own frames reported: {page}");
    assert!(page.contains("93% cached"), "and the cache share of what it read: {page}");
    // The end is the last row's own instant in the reader's own clock. Two
    // turns whose last rows are 3m 22s apart must date themselves the same
    // 3m 22s apart: a row stamped with the clock at load would date both to
    // the moment somebody opened the page.
    let ended = ended_cells(&page);
    assert_eq!(ended.len(), 2, "each row carries an ended cell: {page}");
    assert!(
        ended
            .iter()
            .all(|clock| clock.contains(':') && clock.ends_with(|c: char| c.is_ascii_digit())),
        "and each is a clock rather than a dash: {ended:?}",
    );
    assert!(
        ended.iter().all(|clock| clock.len() > 8),
        "and carries its date, these turns being older than today: {ended:?}",
    );
    assert_eq!(
        seconds_of_clock(&ended[1]) - seconds_of_clock(&ended[0]),
        3 * 60 + 22,
        "the rows date themselves from the transcript's own clocks: {ended:?}",
    );
}

/// A turn settling stays one row. The model's last assistant frame settles
/// the turn as far as its rows are concerned, and it arrives a frame before
/// the result does, so the frame it lands in must not draw a second row
/// beside the live one and take it away again.
#[tokio::test]
async fn a_running_turn_draws_the_live_row_and_not_one_beside_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: running_frame(),
    });
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: finished_assistant_frame("m1", "all done"),
    });
    // Read until the frame's own prose is in: the region arrives in chunks,
    // and a count taken off a half-received one reads as an absence.
    let seen = event_carrying(stream, "all done").await;
    let running = seen.rsplit("event: session").next().expect("the running region");

    assert_eq!(
        running.matches("class=\"turninfo\"").count(),
        1,
        "the turn that has not settled draws one row: {running}",
    );
    assert!(
        running.contains("data-k=\"turn-live\""),
        "and it is the live one, still counting: {running}",
    );
    assert!(running.contains("all done"), "with the frame the model just wrote: {running}");
}

/// The result lands the live row into the settled one: same row, one row,
/// now carrying the numbers the CLI reported.
#[tokio::test]
async fn a_settled_turn_replaces_the_live_row_it_ran_as() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let settled = captured_results("monitor_persistent_stream").pop().expect("the captured turn");

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: running_frame(),
    });
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: finished_assistant_frame("m1", "all done"),
    });
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: settled,
    });
    // Read until the settled row's own clock is in: the region arrives in
    // chunks, and a count taken off a half-received one reads as an absence.
    let seen = event_carrying(stream, "41.0s").await;
    let done = seen.rsplit("event: session").next().expect("the settled region");

    assert_eq!(
        done.matches("class=\"turninfo\"").count(),
        1,
        "one row once it settles, not two: {done}",
    );
    assert!(!done.contains("data-k=\"turn-live\""), "the live one is gone: {done}");
    assert!(done.contains("41.0s"), "and the settled row is the CLI's own: {done}");
}

/// The model's last frame of a turn: prose, and the stop that ends it.
fn finished_assistant_frame(id: &str, text: &str) -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "assistant",
        "uuid": id,
        "timestamp": "2026-04-22T04:18:08.000Z",
        "message": {
            "id": id,
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
        },
        "session_id": "s",
    }))
    .expect("an assistant frame")
}

/// A question the assistant asked draws with what was picked and what was
/// typed. Both live on the row's own record of the result, which the read
/// carries: the tool-result block beside it holds one English sentence, and
/// nothing in it says which words were a label and which were typed.
#[tokio::test]
async fn an_answered_question_draws_its_answer_on_a_fresh_page() {
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
                r#"{"type":"assistant","timestamp":"2026-04-22T04:15:27.000Z","message":{"id":"msg_1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_q","name":"AskUserQuestion","input":{"questions":[{"question":"Which colour do you prefer?","header":"Colour","options":[{"label":"Blue","description":"the colder one"},{"label":"Red","description":"the warmer one"}],"multiSelect":false}]}}],"stop_reason":"tool_use"}}"#,
                r#"{"type":"user","timestamp":"2026-04-22T04:15:31.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_q","content":"The user answered: \"Which colour do you prefer?\"=\"Blue\""}]},"toolUseResult":{"questions":[{"question":"Which colour do you prefer?","header":"Colour","options":[{"label":"Blue"},{"label":"Red"}],"multiSelect":false}],"answers":{"Which colour do you prefer?":"Blue"}}}"#,
                r#"{"type":"assistant","timestamp":"2026-04-22T04:18:08.000Z","message":{"id":"msg_2","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"Blue it is."}],"stop_reason":"end_turn"}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains("Which colour do you prefer?"), "the card draws the question: {page}");
    assert!(
        page.contains("class=\"picked\">Blue</span>"),
        "and the option that was picked, which only the row's own record names: {page}",
    );
    assert!(
        !page.contains("class=\"picked\">Red</span>"),
        "while the one that was not picked is not drawn as one: {page}",
    );
}

/// The wall clock on each settled row's `ended` cell, in the order the rows
/// are drawn.
fn ended_cells(page: &str) -> Vec<String> {
    page.split("<b>ended</b>")
        .skip(1)
        .map(|rest| rest.split('<').next().unwrap_or_default().to_owned())
        .collect()
}

/// The `HH:MM:SS` a cell ends on, as seconds since midnight, so two clocks
/// can be compared across a minute or an hour boundary whatever date each
/// carries.
fn seconds_of_clock(clock: &str) -> i64 {
    let tail = &clock[clock.len().saturating_sub(8)..];
    let parts: Vec<i64> = tail.split(':').filter_map(|part| part.parse().ok()).collect();
    let [hours, minutes, seconds] = parts[..] else {
        panic!("not a wall clock: {clock}");
    };
    hours * 3_600 + minutes * 60 + seconds
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
        page.contains("htmx:afterSwap"),
        "and the swap carries what a reader opened or closed across itself: {page}",
    );
    // The handler keeps that state by key, so a section without one is a
    // section whose state the next swap drops.
    // Every pane rule names the app the boxes are siblings of, by the hop
    // the markup actually has. A rule reaching through the swapped region
    // matches nothing once the app is not inside it, and the handles go
    // dead at every width with nothing but a browser to say so.
    for rule in sheet.split("#l:checked ~ ").skip(1).chain(sheet.split("#r:checked ~ ").skip(1)) {
        let target = rule.split([' ', ',', '{']).next().unwrap_or_default();
        assert_eq!(
            target, ".app",
            "a pane rule reaches the app in one hop, got {target:?} in {rule:.60}",
        );
    }
    for block in page.split("<details").skip(1) {
        let head = block.split('>').next().unwrap_or_default();
        assert!(head.contains("data-k="), "every section carries a key: <details{head}>");
    }

    let mut reads = sheet.match_indices(":checked").peekable();
    assert!(reads.peek().is_some(), "the sheet reads the boxes");
    for (at, _) in reads {
        assert!(
            sheet[at..].starts_with(":checked ~ .app"),
            "every rule that reads a box reaches the app, which the box is a sibling of: {}",
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
    fleet.emit(forge_server::SessionUpdate::SessionReplaced {
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

/// One thinking block's worth of estimate, as the CLI reports it: the delta
/// is what a turn's total is summed from, so one frame here is one block, and
/// each block carries the uuid that makes it its own frame.
fn thinking_frame(nth: u64, tokens: u64) -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "system",
        "subtype": "thinking_tokens",
        "estimated_tokens": tokens,
        "estimated_tokens_delta": tokens,
        "uuid": format!("think-{nth}"),
        "session_id": "s",
    }))
    .expect("a thinking frame")
}

/// Two blocks in one turn, the second restarting the wire's counter the way
/// it does at every block boundary: the row sums them rather than reading the
/// counter, which would step backwards.
#[tokio::test]
async fn the_live_row_sums_every_thinking_block() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    for msg in [running_frame(), thinking_frame(1, 161), thinking_frame(2, 189)] {
        fleet.emit(forge_server::SessionUpdate::ChatAppended {
            key: SessionSlot::lead("Busytools", "forge"),
            msg,
        });
    }
    // Read until the estimate the two blocks add up to, rather than for a
    // fixed number of events: the page's stream carries whatever else
    // redraws it, so a count is a claim about traffic rather than about the
    // row.
    let region =
        read_until(stream, "the live row's estimate", &|seen| seen.contains("thinking 350")).await;

    assert!(
        region.contains("thinking 350"),
        "the blocks sum rather than the last block standing alone: {region}",
    );
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

/// The live row draws inside the work block the settled row will land in,
/// not in a block of its own. That is what keeps it still: a block of its
/// own sits a block padding lower, so the row moves down by twelve pixels
/// the moment the turn settles, under the reader's cursor.
#[tokio::test]
async fn the_live_row_draws_where_the_settled_one_will_land() {
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
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/tmp/src/lib.rs"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"one\ntwo"}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: running_frame(),
    });
    let region = next_session_event(stream).await.expect("the state redraws the region");

    let row = region.find("data-k=\"turn-live\"").expect("the live row is drawn");
    let block = region[..row].rfind("class=\"work\"").expect("a work block above it");
    assert!(
        region[block..row].contains("class=\"kind\""),
        "the live row shares the block with the work the turn did: {}",
        &region[block..row],
    );
    assert!(
        !region[..row].trim_end().ends_with("class=\"work\">"),
        "rather than opening a block of its own for itself: {}",
        &region[block..row],
    );
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
    for msg in [running_frame(), thinking_frame(1, 434)] {
        fleet.emit(forge_server::SessionUpdate::ChatAppended {
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
    for msg in [thinking_frame(1, 434), settled] {
        fleet.emit(forge_server::SessionUpdate::ChatAppended {
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
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: frame,
    });
    let region = next_session_event(stream).await.expect("the turn redraws the region");

    assert!(region.contains("<b>cache</b>0 read"), "the zero it did spend: {region}");
    assert!(region.contains("<b>wrote</b>13,939"), "beside the count it did spend: {region}");
    assert!(region.contains("13k written"), "and the row's own chip keeps it: {region}");
}

/// A mutation's leaf draws the edit glyph and starts open, which is what the
/// fold's one `edit` row feeds. Both are decided in the view, and reverting
/// either to the four tool names behind that row leaves every mutation's
/// leaf collapsed and generic, which is what the mockup forbids.
#[tokio::test]
async fn a_mutation_leaf_draws_the_edit_glyph_and_starts_open() {
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
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_9","name":"Edit","input":{"file_path":"/tmp/src/lib.rs","old_string":"one","new_string":"two"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_9","content":"the file has been updated"}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        page.contains("<div class=\"knd\"><svg class=\"ic gl\"><use href=\"#i-edit\">"),
        "the family row draws the mutation's glyph: {page}",
    );
    assert!(
        page.contains("<details class=\"leaf\" open data-k=\"leaf-toolu_9\">"),
        "and its own leaf opens on the diff without being asked: {page}",
    );
}

/// A source file a call read is drawn as the mockup draws it: a code block
/// with its language named and its tokens classed, rather than the file as
/// plain rows.
#[tokio::test]
async fn a_source_file_is_drawn_highlighted() {
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
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/tmp/src/family.rs"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"/// The group a call belongs to.\npub enum ToolFamily {\n    Read,\n}"}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains("class=\"code\""), "the block the mockup draws: {page}");
    assert!(page.contains("<div class=\"lang\">rust</div>"), "naming its language: {page}");
    assert!(page.contains("<span class=\"k\">"), "with its keywords classed: {page}");
    assert!(page.contains("ToolFamily"), "and the file's own text: {page}");
}

/// A command's own line leads its output. The call's title is the
/// description when it carries one, so without this the command it ran is
/// drawn nowhere at all.
#[tokio::test]
async fn a_command_leads_its_own_output() {
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
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"just check","description":"run the gates"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"   Compiling forge-web v1.0.91\n    Finished in 41.2s"}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        page.contains("<span class=\"pfx\">$</span> just check"),
        "the command, under its own prompt: {page}",
    );
    assert!(page.contains("Finished in 41.2s"), "and the output it wrote below it: {page}");
    assert!(
        page.contains("run the gates"),
        "while the row above still names the call by its description: {page}",
    );
}

/// A command with no description is named by the row above, and the row caps
/// that title at three lines - so the body draws the command too, and it is
/// the only place the whole of a long one stays readable on the page.
#[tokio::test]
async fn a_command_the_row_already_names_is_still_readable_in_its_body() {
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
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"just check"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"    Finished in 41.2s"}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains(">just check</span>"), "the row names the command: {page}");
    assert!(
        page.contains("<span class=\"pfx\">$</span> just check"),
        "and the body draws it, where a capped title cannot reach: {page}",
    );
    assert!(page.contains("Finished in 41.2s"), "while the output it wrote is still drawn: {page}");
}

/// A command whose description ends in an extension is still a command: its
/// output is its own lines under the prompt, not a code block, and the
/// command it ran is drawn.
#[tokio::test]
async fn a_command_whose_description_looks_like_a_path_is_still_a_command() {
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
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"just check","description":"regen the fixtures in spec.rs"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"    Finished in 41.2s"}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        page.contains("<span class=\"pfx\">$</span> just check"),
        "the command is drawn, not hidden behind a title that reads as a path: {page}",
    );
    assert!(
        !page.contains("class=\"code\""),
        "and its output is not drawn as a source file: {page}",
    );
}

/// A search call's hits are drawn one row each: the line number, the file,
/// and the line with the pattern marked.
#[tokio::test]
async fn a_search_result_is_drawn_as_its_hits() {
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
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_1","name":"Grep","input":{"pattern":"KindRow"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"crates/forge-server/src/grouping.rs:184: pub fn kind_row(x: KindRow)"}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains("class=\"searchhit\""), "the row a hit draws on: {page}");
    assert!(page.contains("<span class=\"ln\">184:</span>"), "with its line number: {page}");
    assert!(
        page.contains("<span class=\"fl\">crates/forge-server/src/grouping.rs</span>"),
        "and the file it is in: {page}",
    );
    assert!(
        page.contains("pub fn kind_row(x: <span class=\"hit\">KindRow</span>)"),
        "the line it sits in, with the pattern marked and the rest left as it came: {page}",
    );
}

/// A turn's hooks collapse to the chip the mockup draws, with what each one
/// ran and how long it took behind it.
#[tokio::test]
async fn a_turns_hooks_are_drawn_as_the_chip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    // Built as the decoder's typed variant rather than from JSON: the wire
    // reaches this one through the subtype dispatch, not through serde.
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: SessionSlot::lead("Busytools", "forge"),
        msg: forge_primitives::Message::StopHookSummary {
            actions: 2,
            hook_infos: vec![
                forge_primitives::messages::StopHookInfo {
                    command: "just fmt".to_owned(),
                    duration_ms: Some(1400),
                },
                forge_primitives::messages::StopHookInfo {
                    command: "just check".to_owned(),
                    duration_ms: Some(62000),
                },
            ],
            has_output: true,
            level: "suggestion".to_owned(),
            prevented_continuation: false,
            stop_reason: String::new(),
            tool_use_id: String::new(),
            parent_tool_use_id: None,
            session_id: "s".to_owned(),
            uuid: "hooks-1".to_owned(),
        },
    });
    let region = next_session_event(stream).await.expect("the frame redraws the region");

    assert!(region.contains("class=\"hooks\""), "the chip: {region}");
    assert!(region.contains("hook summary \u{b7} 2 actions"), "with its count: {region}");
    assert!(region.contains("just fmt \u{b7} 1.4s"), "and what each one ran: {region}");
    assert!(region.contains("just check \u{b7} 1m 02s"), "with how long it took: {region}");
}

/// The same row, on a page nobody watched arrive. A turn's hooks are on disk
/// as a `system/stop_hook_summary` row and the read dropped every `system`
/// row, so the chip was a surface that existed only while the page was
/// live. A fresh load has the identical row and draws it.
#[tokio::test]
async fn a_turns_hooks_draw_on_a_fresh_load() {
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
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"the layout pass is in"}]}}"#,
                r#"{"type":"system","subtype":"stop_hook_summary","hookCount":2,"hookInfos":[{"command":"just fmt","durationMs":1400},{"command":"just check","durationMs":62000}],"hookErrors":[],"hasOutput":true,"level":"suggestion","preventedContinuation":false,"stopReason":"","toolUseID":"toolu_hook","uuid":"hooks-1","sessionId":"s"}"#,
                r#"{"type":"assistant","message":{"id":"m2","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"the sweep is done"}]}}"#,
                r#"{"type":"system","subtype":"stop_hook_summary","hookCount":1,"hookInfos":[{"command":"just tidy","durationMs":300}],"hookErrors":[],"hasOutput":true,"level":"suggestion","preventedContinuation":false,"stopReason":"","toolUseID":"toolu_hook2","uuid":"hooks-2","sessionId":"s"}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains("the layout pass is in"), "the turn the hooks belong to draws: {page}");
    assert!(
        page.matches("class=\"hooks\"").count() == 2,
        "one chip per turn whose hooks ran, which is what the mockup draws: {page}",
    );
    assert!(
        page.contains("hook summary \u{b7} 2 actions"),
        "with the count the row carries: {page}",
    );
    assert!(page.contains("just fmt \u{b7} 1.4s"), "and what each one ran: {page}");
    assert!(page.contains("hook summary \u{b7} 1 action"), "or its singular: {page}");
    assert!(page.contains("just tidy \u{b7} 0.3s"), "for the turn after it: {page}");
}

/// A sub-agent's frames reach this page's stream, and the chat draws none of
/// them. The drawing says the SUBAGENTS section is the only surface an agent
/// has, the terminal suppresses the same frames, and the inspector's note on
/// this very page repeats it. The session's own line rides in the same
/// region as the control, so a chat that drew nothing at all would not pass.
#[tokio::test]
async fn the_chat_draws_none_of_a_sub_agents_frames() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    let key = SessionSlot::lead("Busytools", "forge");
    for (text, parent) in [
        ("a dispatched agent thinking out loud", Some("toolu_dispatch")),
        ("the session's own line", None),
    ] {
        fleet.emit(forge_server::SessionUpdate::ChatAppended {
            key: key.clone(),
            msg: assistant_saying(text, parent),
        });
    }
    let region = nth_session_event(stream, 3).await.expect("the region after both frames");

    assert!(region.contains("the session's own line"), "the session's own prose draws: {region}");
    assert!(
        !region.contains("a dispatched agent thinking out loud"),
        "and a dispatched agent's does not: {region}",
    );
}

/// One assistant frame with a line of its own, as the wire sends it.
/// `parent` is the dispatch's tool-use id, which is what makes the frame a
/// sub-agent's.
fn assistant_saying(text: &str, parent: Option<&str>) -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "assistant",
        "session_id": "s",
        "parent_tool_use_id": parent,
        "message": {
            "id": format!("msg-{}", parent.unwrap_or("own")),
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [{"type": "text", "text": text}],
        },
    }))
    .expect("an assistant frame")
}

/// A compaction in flight says so, from the session's own status frame, and
/// stops saying so when the frame says it ended.
#[tokio::test]
async fn a_compaction_in_flight_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    let key = SessionSlot::lead("Busytools", "forge");
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: key.clone(),
        msg: serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "status",
            "status": "compacting",
            "uuid": "status-1",
            "session_id": "s",
        }))
        .expect("a status frame"),
    });
    // An unrelated update follows the status, and the region read is the one
    // after both. A compaction runs for minutes while anything else may land
    // in between, so a flag that lived for one redraw would flash the line
    // and take it away at the tick.
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key,
        msg: user_frame("u-1", "a message while the compaction runs"),
    });
    let region = nth_session_event(stream, 3).await.expect("both updates redraw the region");

    assert!(
        region.contains("a message while the compaction runs"),
        "precondition: the unrelated update is the later of the two: {region}",
    );
    assert!(
        region.contains("Compacting context"),
        "and the compaction is still running, so the line is still drawn: {region}",
    );
}

/// The line goes when the session says the compaction ended, which is the
/// status frame's null rather than a second frame that never comes.
#[tokio::test]
async fn a_compaction_that_ended_stops_saying_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    let key = SessionSlot::lead("Busytools", "forge");
    for (uuid, status) in
        [("status-1", serde_json::json!("compacting")), ("status-2", serde_json::json!(null))]
    {
        fleet.emit(forge_server::SessionUpdate::ChatAppended {
            key: key.clone(),
            msg: serde_json::from_value(serde_json::json!({
                "type": "system",
                "subtype": "status",
                "status": status,
                "uuid": uuid,
                "session_id": "s",
            }))
            .expect("a status frame"),
        });
    }
    let region = nth_session_event(stream, 3).await.expect("both statuses redraw the region");

    assert!(!region.contains("Compacting context"), "the line is gone once it ended: {region}");
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
    fleet.emit(forge_server::SessionUpdate::CronPromptAppended {
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
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
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
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
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
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
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

/// The header states the session's own facts, which no update stream
/// carries: the model it resolved, the effort it runs at, the mode a hook
/// observed and how full its context is.
#[tokio::test]
async fn the_header_states_the_sessions_own_facts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            model: Some(CurrentModel::new("claude-opus-5-5", "Opus 5.5", "Claude Opus 5.5")),
            observed_effort: Some(EffortLevel::High),
            permission_mode: Some(PermissionMode::BypassPermissions),
            context: Some(ContextUsage { percent: Some(41), max_tokens: Some(200_000) }),
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(page.contains(">model<"), "the header names the model: {page}");
    assert!(page.contains("Opus 5.5"), "and states which one: {page}");
    assert!(page.contains(">effort<"), "it names the effort: {page}");
    assert!(page.contains("high"), "and states the level the session runs at: {page}");
    assert!(page.contains(">mode<"), "it names the mode: {page}");
    assert!(page.contains("bypassPermissions"), "and states which one a hook saw: {page}");
    assert!(page.contains(">ctx<"), "it names the context reading: {page}");
    assert!(page.contains("41%"), "and states how full the window is: {page}");
}

/// A session whose hook has not fired yet still has an effort, because
/// forge launched it at one: a header that went blank there would be wrong
/// about every session between its spawn and its first tool call.
#[tokio::test]
async fn the_header_states_the_launched_effort_before_a_hook_reports() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts { configured_effort: Some(EffortLevel::Xhigh), ..ViewFacts::default() },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("xhigh"),
        "a session no hook has reported on states the level it was launched at: {page}",
    );
}

/// One assistant frame carrying a single tool call, from the instance named
/// by `parent` and from the session itself when it is `None`.
fn call_frame(
    id: &str,
    name: &str,
    input: &serde_json::Value,
    parent: Option<&str>,
) -> forge_primitives::Message {
    let mut frame = serde_json::json!({
        "type": "assistant",
        "uuid": format!("u-{id}"),
        "message": {
            "id": "msg_1",
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
        },
        "session_id": "s",
    });
    if let Some(parent) = parent {
        frame["parent_tool_use_id"] = serde_json::Value::String(parent.to_owned());
    }
    serde_json::from_value(frame).expect("an assistant frame carrying one call")
}

/// A page opened after the instance ran draws no card for it. Its transcript
/// holds the dispatch and the CLI's launch acknowledgement and nothing else:
/// the rows that report an end are system rows the scan does not keep, and
/// the instance's own frames live in a sidechain file this read does not
/// open. Nothing says whether it is over, so the page says nothing rather
/// than a check mark under work that may still be running.
#[tokio::test]
async fn a_dispatch_whose_transcript_says_nothing_is_not_drawn() {
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
                r#"{"type":"assistant","uuid":"u-1","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_a","name":"Task","input":{"description":"cli-version","subagent_type":"Explore","prompt":"land it"}}]}}"#,
                r#"{"type":"user","uuid":"u-2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_a","content":[{"type":"text","text":"Async agent launched successfully. (This tool result is internal metadata, never quote or paste any part of it, including the agentId below.)\nagentId: a5f83c2a9b88e4db5"}]}]}}"#,
                r#"{"type":"assistant","uuid":"u-3","message":{"id":"m2","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_b","name":"Task","input":{"description":"web-session-review","subagent_type":"Explore","prompt":"review it"}}]}}"#,
                r#"{"type":"user","uuid":"u-4","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_b","content":[{"type":"text","text":"Async agent launched successfully. (This tool result is internal metadata, never quote or paste any part of it, including the agentId below.)\nagentId: b6f93d3c0b99e5ea6"}]}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        !page.contains("href=\"#i-subagents\""),
        "a launch acknowledgement is not evidence the instance is over: {page}",
    );
    assert!(
        !page.contains("<span class=\"nm\">cli-version</span>"),
        "and nothing claims the instance either way: {page}",
    );
}

/// A running instance draws its own calls under it, in the order it fired
/// them, each named by the tool and what it was aimed at.
#[tokio::test]
async fn a_running_instance_draws_its_own_calls() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let key = SessionSlot::lead("Busytools", "forge");

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: key.clone(),
        msg: call_frame(
            "toolu_a",
            "Task",
            &serde_json::json!({"description": "web-session-review", "subagent_type": "Explore"}),
            None,
        ),
    });
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: key.clone(),
        msg: call_frame(
            "toolu_a1",
            "Read",
            &serde_json::json!({"file_path": "/srv/docs/book/src/ui/chat.md"}),
            Some("toolu_a"),
        ),
    });
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key,
        msg: call_frame(
            "toolu_a2",
            "Bash",
            &serde_json::json!({"command": "cargo nextest run -p forge-web"}),
            Some("toolu_a"),
        ),
    });

    let region = event_carrying(stream, "Bash cargo nextest run -p forge-web").await;

    assert!(
        region.contains("<span class=\"nm\">web-session-review</span>"),
        "the card is named for its instance: {region}",
    );
    assert!(region.contains("running \u{b7} 2 tools"), "and says what it is doing: {region}");
    assert!(
        region.contains("<span class=\"c2\">1 running</span>"),
        "and the section states how much of the session is running: {region}",
    );
    assert!(
        region.contains(
            "<div class=\"tt\"><svg class=\"ic tg\"><use href=\"#i-read\"></svg> Read \
             /srv/docs/book/src/ui/chat.md</div>"
        ),
        "a call's row draws the tool's own icon and its title, which is what the \
         per-call row is for: {region}",
    );
    assert!(
        region.contains(
            "<div class=\"tt\"><svg class=\"ic tg\"><use href=\"#i-bash\"></svg> Bash \
             cargo nextest run -p forge-web</div>"
        ),
        "and the second row draws the second tool's icon rather than the first's: {region}",
    );
    let read = region.find("Read /srv/docs/book/src/ui/chat.md").expect("its first call draws");
    let bash = region.find("Bash cargo nextest run -p forge-web").expect("and its second draws");
    assert!(read < bash, "in the order the instance fired them: {region}");
    assert!(
        !region.contains("<div class=\"settled\">"),
        "a running instance is not drawn as a settled one: {region}",
    );
}

/// The card states when an instance settled, from the instant the CLI's own
/// roster stamped on the frame that ended it - rather than an age counted
/// from whenever the page happened to load - and counts the one call the
/// instance made as one tool rather than one tools.
#[tokio::test]
async fn a_settled_instance_states_when_it_settled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.seed_transcript("Busytools", "forge", "lead", &[]).expect("an empty transcript");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let key = SessionSlot::lead("Busytools", "forge");
    let ended_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("the clock is past the epoch")
        .as_millis()
        .saturating_sub(12 * 60 * 1000);

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: key.clone(),
        msg: call_frame(
            "toolu_a",
            "Task",
            &serde_json::json!({"description": "cli-version", "subagent_type": "Explore"}),
            None,
        ),
    });
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: key.clone(),
        msg: call_frame(
            "toolu_a1",
            "Read",
            &serde_json::json!({"file_path": "/srv/docs/manual.md"}),
            Some("toolu_a"),
        ),
    });
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key: key.clone(),
        msg: serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "t-a",
            "description": "cli-version",
            "uuid": "u-start",
            "session_id": "s",
            "tool_use_id": "toolu_a",
        }))
        .expect("a task_started frame"),
    });
    fleet.emit(forge_server::SessionUpdate::ChatAppended {
        key,
        msg: serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "task_updated",
            "task_id": "t-a",
            "patch": {"status": "completed", "end_time": ended_ms},
            "uuid": "u-upd",
            "session_id": "s",
        }))
        .expect("a task_updated frame carrying an end time"),
    });

    let region = event_carrying(stream, "settled 12m").await;

    assert!(
        region.contains("<span class=\"n\">1 tool \u{b7} settled 12m</span>"),
        "the card counts one call as one tool and states how long ago it settled: {region}",
    );
    assert!(
        region.contains("<div class=\"settled\">"),
        "and draws the settled card rather than a live tail: {region}",
    );
    assert!(
        region.contains("<span class=\"c2\">0 running</span>"),
        "and the section counts what is still running rather than what it holds: {region}",
    );
}

/// The MCP section lists the session's own servers - MCP is configured per
/// session, not per account - under a count of what is behind it.
#[tokio::test]
async fn the_mcp_section_lists_the_sessions_servers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    // The second server reports no scope of its own, which is the case the
    // CLI leaves for an in-process server: its config blob is what names it.
    // The third reports none either and is not one of those, which is the
    // ordinary case: a server the CLI says nothing about.
    let mut sdk = mcp_server("context7", "session", 2);
    sdk.scope = None;
    sdk.config = Some(serde_json::json!({ "type": "sdk" }));
    let mut quiet = mcp_server("playwright", "session", 3);
    quiet.scope = None;
    quiet.config = None;
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            mcp: Some(McpServers {
                servers: vec![mcp_server("forge", "session", 24), sdk, quiet],
                error: None,
            }),
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(page.contains("href=\"#i-mcp\""), "the section renders: {page}");
    assert!(
        page.contains("<span class=\"k\">forge \u{b7} session</span>"),
        "naming the server and the scope it is configured in: {page}",
    );
    assert!(
        page.contains("<span class=\"v\">24 tools</span>"),
        "and how many tools it offers, which is what the terminal's own row says: {page}",
    );
    assert!(
        page.contains("<span class=\"k\">context7 \u{b7} sdk</span>"),
        "the next one, with the scope its config blob names: {page}",
    );
    assert!(
        page.contains("<span class=\"k\">playwright \u{b7} session</span>"),
        "and one the CLI says nothing about, which is the session's own: {page}",
    );
    assert!(page.contains("<span class=\"c2\">3</span>"), "under a count of them: {page}");
}

/// A server that is not up says why in the place its tool count would be,
/// and a count of one reads as one rather than as one tools.
#[tokio::test]
async fn an_mcp_row_states_its_state_where_a_tool_count_would_go() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    let mut refused = serde_json::from_value::<McpServerStatus>(serde_json::json!({
        "name": "playwright",
        "status": "failed",
        "error": "Server does not exist",
    }))
    .expect("a failed server");
    refused.scope = Some("user".to_owned());
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            mcp: Some(McpServers {
                servers: vec![refused, mcp_server("one-tool", "session", 1)],
                error: None,
            }),
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("<span class=\"v\">Server does not exist</span>"),
        "a server that failed says why, where the count would be: {page}",
    );
    assert!(
        page.contains("<span class=\"v\">1 tool</span>"),
        "and a count of one reads as one: {page}",
    );
}

/// A failed MCP read says so rather than rendering a session with no
/// servers: the snapshot carries an empty list, and an empty list with no
/// reason reads as "nothing configured".
#[tokio::test]
async fn a_failed_mcp_read_states_why_it_is_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            mcp: Some(McpServers {
                servers: Vec::new(),
                error: Some("the CLI refused".to_owned()),
            }),
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(page.contains("the CLI refused"), "the reason the read failed is drawn: {page}");
}

/// The processes section draws the walk the core holds: what is running,
/// what it is under, its pid and the memory it holds.
#[tokio::test]
async fn the_processes_section_draws_the_walk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            process_snapshot: Some(ProcessSnapshot {
                // The scan returns entries by memory, descending, so a
                // child holding more than its parent is listed before it:
                // the section has to draw the tree, not that order.
                processes: vec![
                    process(4244, 4242, "big-rustc", "rustc --crate-name forge_web", 900),
                    process(4242, 4000, "cargo", "cargo nextest run", 412),
                    process(4243, 4242, "cc", "cc -O2 -o build/obj.o", 88),
                ],
                scanned_at: std::time::SystemTime::UNIX_EPOCH,
            }),
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(page.contains("href=\"#i-processes\""), "the section renders: {page}");
    assert!(
        page.contains("<span class=\"k\">cargo nextest run</span>"),
        "naming the command the supervisor runs: {page}",
    );
    assert!(
        page.contains("<span class=\"k\">&nbsp;&nbsp;cc -O2 -o build/obj.o</span>"),
        "and its child's command, indented under it: {page}",
    );
    assert!(page.contains("412 MB"), "with the memory it holds: {page}");
    assert!(page.contains("4242"), "and the pid it runs under: {page}");
    assert!(page.contains("<span class=\"c2\">3</span>"), "under a count of the rows: {page}");
    let cargo =
        page.find("<span class=\"k\">cargo nextest run</span>").expect("the parent is listed");
    let child =
        page.find("<span class=\"k\">&nbsp;&nbsp;cc -O2 -o build/obj.o</span>").expect("the child");
    assert!(cargo < child, "a parent is drawn before the child it indents: {page}");
    let heavy = page.find("rustc --crate-name forge_web").expect("the heavier child is listed");
    assert!(
        cargo < heavy,
        "and a child heavier than its parent still follows it, rather than the scan's order: {page}",
    );
}

/// A row names the command its process is running rather than the name the
/// OS gives it: the name alone says nothing about which node process it is,
/// and the terminal's own row draws the cmdline for that reason. A shell
/// wrapper's chrome is not a headline either, so its inner command is what
/// shows.
#[tokio::test]
async fn the_processes_rows_name_the_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            process_snapshot: Some(ProcessSnapshot {
                processes: vec![
                    process(5000, 4000, "node", "node /opt/ctx7/server.js --stdio", 64),
                    // The shape claude wraps a Bash call in, which is what
                    // the terminal unwraps: the inner command sits between
                    // `eval '` and the ` < /dev/null` redirect.
                    process(
                        5001,
                        4000,
                        "zsh",
                        "/bin/zsh -c source /tmp/snap.sh && eval 'cargo nextest run' \
                         < /dev/null && pwd -P >| /tmp/claude-cwd",
                        12,
                    ),
                ],
                scanned_at: std::time::SystemTime::now(),
            }),
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("<span class=\"k\">node /opt/ctx7/server.js --stdio</span>"),
        "the row names the command with its executable's path stripped: {page}",
    );
    assert!(
        page.contains("<span class=\"k\">cargo nextest run</span>"),
        "a shell wrapper shows the command it wraps rather than its own chrome: {page}",
    );
}

/// The walk is only ever taken for the session a view is looking at, so a
/// slot nobody is looking at serves whatever was last left there. The
/// rows say how old that is, because a tree from an hour ago drawn the
/// same way as one from a second ago is a wrong answer rather than an
/// old one.
#[tokio::test]
async fn the_processes_section_states_the_age_of_the_walk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            process_snapshot: Some(ProcessSnapshot {
                processes: vec![process(4242, 4000, "cargo", "cargo nextest run", 412)],
                scanned_at: std::time::SystemTime::now() - std::time::Duration::from_secs(120),
            }),
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("walked 2m ago"),
        "the rows say when the walk behind them was taken: {page}",
    );
}

/// The monitors section is where monitors live: the chat does not carry
/// them, so a running monitor with nothing drawing it would be invisible.
#[tokio::test]
async fn the_monitors_section_draws_the_live_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            monitors: vec![
                MonitorRecord {
                    tool_use_id: "tu-live".to_owned(),
                    task_id: Some("t-live".to_owned()),
                    description: "ci-watch".to_owned(),
                    command: "gh run watch 18234567".to_owned(),
                    persistent: true,
                    timeout_ms: 0,
                    status: MonitorStatus::Running,
                    output_file: None,
                    ended_at: None,
                },
                MonitorRecord {
                    tool_use_id: "tu-done".to_owned(),
                    task_id: Some("t-done".to_owned()),
                    description: "deploy-gate".to_owned(),
                    command: "gh run watch 2".to_owned(),
                    persistent: false,
                    timeout_ms: 0,
                    status: MonitorStatus::Completed,
                    output_file: None,
                    ended_at: None,
                },
            ],
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(page.contains("href=\"#i-monitors\""), "the section renders: {page}");
    assert!(page.contains("ci-watch"), "naming the monitor: {page}");
    assert!(page.contains("gh run watch 18234567"), "and the command it watches: {page}");
    assert!(page.contains("persistent"), "and whether it outlives its event: {page}");
    assert!(page.contains("1 running"), "under a count of what is still live: {page}");
    assert!(page.contains("deploy-gate"), "with the settled one listed too: {page}");
    // The state class rides the mark itself, the way the mockup draws it,
    // rather than a span around it.
    assert!(
        page.contains("<svg class=\"ic st\">"),
        "a settled monitor's mark carries its own state class: {page}",
    );
}

/// The watched command's own output is what the section is opened for. The
/// CLI streams it to a file rather than over the wire and names that file
/// on the notification that ends the monitor, so the output is drawn under
/// the command of the card that ended.
#[tokio::test]
async fn a_settled_monitor_draws_its_commands_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("ci-watch.out");
    std::fs::write(&out, "build \u{b7} in_progress\nlint \u{b7} success\ndeploy \u{b7} queued\n")
        .expect("seed the watched command's output");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            monitors: vec![MonitorRecord {
                tool_use_id: "tu-done".to_owned(),
                task_id: Some("t-done".to_owned()),
                description: "ci-watch".to_owned(),
                command: "gh run watch 18234567".to_owned(),
                persistent: false,
                timeout_ms: 0,
                status: MonitorStatus::Completed,
                output_file: Some(out.to_string_lossy().into_owned()),
                ended_at: None,
            }],
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    let command = page.find("gh run watch 18234567").expect("the command row renders");
    let first = page.find("build \u{b7} in_progress").expect("the first output line renders");
    let last = page.find("deploy \u{b7} queued").expect("and the last one");
    assert!(command < first, "the output sits under the command it came from: {page}");
    assert!(first < last, "and the lines keep the order the command wrote them in: {page}");
}

/// A running monitor names no file, and its card draws its command and no
/// output. The record is created without a path and the only frame that
/// carries one settles the monitor, so this is not a gap in the drawing:
/// there is nothing yet to draw.
#[tokio::test]
async fn a_running_monitor_draws_only_its_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            monitors: vec![MonitorRecord {
                tool_use_id: "tu-live".to_owned(),
                task_id: Some("t-live".to_owned()),
                description: "ci-watch".to_owned(),
                command: "gh run watch 18234567".to_owned(),
                persistent: true,
                timeout_ms: 0,
                status: MonitorStatus::Running,
                output_file: None,
                ended_at: None,
            }],
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("gh run watch 18234567"),
        "a monitor still watching draws the command it watches: {page}",
    );
    assert!(page.contains("ci-watch"), "and its own row: {page}");
    assert!(
        !page.contains("class=\"settled\""),
        "and no settled line, which nothing has said: {page}",
    );
}

/// A settled monitor says how long ago it settled. The record carries the
/// instant the wire stamped on the transition that ended it, so the row
/// reads `completed 12m` rather than only `completed`.
#[tokio::test]
async fn a_settled_monitor_states_the_age_of_its_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            monitors: vec![MonitorRecord {
                tool_use_id: "tu-done".to_owned(),
                task_id: Some("t-done".to_owned()),
                description: "deploy-gate".to_owned(),
                command: "gh run watch 2".to_owned(),
                persistent: false,
                timeout_ms: 0,
                status: MonitorStatus::Completed,
                output_file: None,
                ended_at: Some(
                    std::time::SystemTime::now() - std::time::Duration::from_secs(12 * 60),
                ),
            }],
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("<span class=\"n\">completed 12m</span>"),
        "the settled row says how long ago the watched command ended: {page}",
    );
}

/// A settled monitor whose transition carried no instant still states that
/// it settled. The age is the extra, not the answer.
#[tokio::test]
async fn a_settled_monitor_without_an_instant_still_states_its_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts {
            monitors: vec![MonitorRecord {
                tool_use_id: "tu-done".to_owned(),
                task_id: Some("t-done".to_owned()),
                description: "deploy-gate".to_owned(),
                command: "gh run watch 2".to_owned(),
                persistent: false,
                timeout_ms: 0,
                status: MonitorStatus::Completed,
                output_file: None,
                ended_at: None,
            }],
            ..ViewFacts::default()
        },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("<span class=\"n\">completed</span>"),
        "a settled monitor with no instant states the end and no more: {page}",
    );
}

/// A snapshot that came back with no servers and no failure is a session
/// with nothing configured, not a read that failed: the section stays
/// away rather than drawing a row that says nothing.
#[tokio::test]
async fn an_empty_mcp_snapshot_draws_no_section() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let lead = SessionSlot::lead("Busytools", "forge");
    fleet.seed_view_facts(
        &lead,
        ViewFacts { mcp: Some(McpServers::default()), ..ViewFacts::default() },
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        !page.contains("href=\"#i-mcp\""),
        "a session with no servers has no section to draw: {page}",
    );
}

/// None of the four invents content: a session that has reported nothing
/// draws no section, rather than an empty one saying it has.
#[tokio::test]
async fn the_four_sections_are_absent_when_the_session_reports_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    for icon in ["i-subagents", "i-mcp", "i-processes", "i-monitors"] {
        assert!(
            !page.contains(&format!("href=\"#{icon}\"")),
            "no {icon} section without anything behind it: {page}",
        );
    }
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
            forge_primitives::AvailableCommand::new("memory", "Edit project memory"),
            forge_primitives::AvailableCommand::new("status", "Show the session status"),
        ],
        Vec::new(),
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, page) = composer(&config, "/m").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains("class=\"ac\""), "the popover opens on the trigger: {page}");
    assert!(page.contains("commands"), "and names the list it is showing: {page}");
    assert!(page.contains("/<em>m</em>emory"), "with the typed span marked: {page}");
    assert!(page.contains("Edit project memory"), "and the row's own description: {page}");
    assert!(
        page.contains("class=\"it sel\""),
        "and the first row is the one a key would take: {page}"
    );
    let forge_row = page.find("/<em>m</em>ode").expect("forge's own row is drawn");
    let advertised_row = page.find("/<em>m</em>emory").expect("the CLI's row is drawn");
    assert!(
        forge_row < advertised_row,
        "and forge's own commands lead, so the row a key would take is one of them: {page}",
    );
}

/// The list carries forge's own commands beside the CLI's. `/diff` is one
/// the CLI never advertises, so a popover built from the advertised list
/// alone cannot offer it however the session is set up.
#[tokio::test]
async fn the_command_list_carries_forges_own_commands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "/diff").await;

    assert!(page.contains("/<em>diff</em>"), "forge's own command is offered: {page}");
    assert!(
        page.contains("Review changes in a full-screen diff overlay"),
        "with the description the shared table carries: {page}",
    );
}

/// One command is one row. forge handles `/model` itself, so the CLI's
/// `model` is not drawn beside it - and the row that is drawn is forge's,
/// which is the half a list built from the CLI's own advertisement cannot
/// show.
#[tokio::test]
async fn a_command_forge_handles_shadows_the_clis_own_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.advertise(
        &SessionSlot::lead("Busytools", "forge"),
        vec![forge_primitives::AvailableCommand::new("model", "Switch model")],
        Vec::new(),
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "/model").await;

    assert!(page.contains("Show / set session model"), "forge's row is the one drawn: {page}");
    assert_eq!(
        page.matches("/<em>model</em>").count(),
        1,
        "and the CLI's copy is not drawn beside it: {page}",
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
    // The header counts the list behind the rows: forge's table, plus the
    // one advertised command that is not a name forge handles itself.
    let listed = forge_server::commands::FORGE_COMMANDS.len() + 1;
    assert!(
        page.contains(&format!(">{listed}<")),
        "while the header counts the list the rows came from: {page}",
    );
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
    assert!(!page.contains("class=\"k\""), "and no keys are advertised with no draft: {page}");
}

/// A seat cannot take input while it is held on a prompt, so the box is
/// replaced by the reason rather than drawn as an input that would drop
/// what was typed into it, and the field it was drawn with is kept out of
/// sight rather than removed. Catches a composer that renders live-looking
/// controls over a session that is not running.
#[tokio::test]
async fn a_seat_with_nothing_running_gets_the_reason_not_a_box() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _content_type, page) = get(&config, "/session/Personal/dotfiles/lead").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(page.contains("class=\"blocked\""), "the slot says why it takes no input: {page}");
    assert!(
        page.contains("</span>not running</span>"),
        "in the composer's own line, which is the same wording the chat column uses: {page}",
    );
    let blocked = page.find("class=\"blocked\"").expect("the reason the box is not drawn");
    let field = page.find("id=\"draft\"").expect("the field it was drawn with");
    assert!(field > blocked, "kept parked inside the reason rather than drawn as a box: {page}");
    assert!(page[field..].contains("hx-preserve"), "and marked so the browser keeps it: {page}");
}

/// The box's send is a control that acts: it posts the draft to the seat,
/// and the box's own keys are the ones that work.
#[tokio::test]
async fn the_box_offers_the_keys_it_honours() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "push it once CI is green").await;

    assert!(page.contains("push it once CI is green"), "the draft is the box's content: {page}");
    assert!(
        page.contains("hx-post=\"/session/Busytools/forge/lead/send\""),
        "the control posts to the seat, not to the region it lives in: {page}",
    );
    assert!(page.contains("</span> send"), "and the keys it honours are named: {page}");
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

fn ended(generation: u64, outcome: forge_server::surface::DictateOutcome) -> SessionUpdate {
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
        forge_server::surface::DictateOutcome::NoAudio { peak_db: -38.2, seconds: 4 },
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
        forge_server::surface::DictateOutcome::Landed {
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
        forge_server::surface::DictateOutcome::NoAudio { peak_db: -38.2, seconds: 4 },
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

fn wire(value: serde_json::Value) -> forge_primitives::permission_interaction::PermissionRequest {
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
fn permission() -> forge_primitives::permission_interaction::PermissionRequest {
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
    question_from("tu-2")
}

/// The same question asked by a named call, so a test can hold two of them
/// apart: what the reader typed for one is not an answer to the next.
fn question_from(tool_id: &str) -> forge_primitives::question::QuestionRequest {
    serde_json::from_value(serde_json::json!({
        "tool_call": tool_call(tool_id, "AskUserQuestion", &serde_json::json!({})),
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
    assert!(!page.contains("class=\"box\""), "and the box itself is gone: {page}");
    assert!(page.contains("git push origin polish/rate-limit-chip"), "the call is named: {page}");
    assert!(page.contains("not on the allow list"), "with the CLI's own reason: {page}");
    let allow = page.find("Allow once").expect("the first option");
    let always = page.find("Allow always for Bash").expect("the second");
    let deny = page.find("Deny").expect("the deny option");
    let notes = page.find("Tell Claude something else").expect("the escape hatch");
    assert!(allow < always && always < deny && deny < notes, "in the order the CLI built: {page}");
}

/// An option is a control that answers: one button per option, each posting
/// the option it names rather than an outcome of its own.
#[tokio::test]
async fn every_dock_option_is_a_control_that_answers() {
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

    assert_eq!(page.matches("<button class=\"lbl\"").count(), 5, "one control per option: {page}");
    assert_eq!(
        page.matches("/session/Busytools/forge/lead/answer").count(),
        5,
        "each posting an answer to the seat: {page}",
    );
    assert!(
        page.contains("&quot;option_id&quot;:&quot;edits&quot;"),
        "naming the option rather than the outcome: {page}",
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

/// A view that attached after the prompt landed still draws what it offers.
/// This is the case the dock exists for - the stream is a mirror with no
/// backlog, so the update that carried the request is long gone, and the
/// request the core kept beside the answer's oneshot is what is left.
#[tokio::test]
async fn a_view_that_attached_late_still_draws_the_options() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Question);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    // Nothing is emitted: this view was not there when the prompt landed.
    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("class=\"dock\""), "the core says a prompt waits: {page}");
    assert!(page.contains("Which environment?"), "and the question it asked: {page}");
    assert!(page.contains("Staging"), "and the options it offered: {page}");
    assert!(
        !page.contains("its options arrived before this view attached"),
        "so nothing is missing from it: {page}",
    );
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
    let blocked = page.find("class=\"blocked\"").expect("the reason the box is not drawn");
    let field = page.find("id=\"draft\"").expect("the field it was drawn with");
    assert!(field > blocked, "kept parked inside the reason rather than drawn as a box: {page}");
    assert!(page[field..].contains("hx-preserve"), "and marked so the browser keeps it: {page}");
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
    let blocked = page.find("class=\"blocked\"").expect("the reason the box is not drawn");
    let field = page.find("id=\"draft\"").expect("the field it was drawn with");
    assert!(
        field > blocked,
        "and the box is gone while it runs, with the reader's field parked inside the reason: {page}",
    );
    assert!(page[field..].contains("hx-preserve"), "and marked so the browser keeps it: {page}");

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

/// A seat that is up claims no sign-in. The guard is the arm that matters
/// most: without it every ordinary box says a sign-in is needed, and the
/// line the reader is meant to act on is the one they learn to ignore.
#[tokio::test]
async fn an_ordinary_box_claims_no_sign_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("id=\"draft\""), "precondition: the box is drawn: {page}");
    assert!(!page.contains("class=\"hint"), "and claims nothing above it: {page}");
    assert!(!page.contains("Authentication required"), "no sign-in is needed: {page}");
}

/// A hint with no description from the wire falls back to the command that
/// fixes it, rather than leaving the reader with a state and no way out.
#[tokio::test]
async fn the_sign_in_hint_falls_back_to_the_command_that_fixes_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.await_login(&lead());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::AuthRequired {
        key: lead(),
        method_name: "claude.ai".to_owned(),
        method_description: String::new(),
    });
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(page.contains("Authentication required"), "the state is named: {page}");
    assert!(
        page.contains("Run `claude auth login`"),
        "and the way out is, when the wire names none: {page}",
    );
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
    let bound = declaration(&sheet, ".ac .rows", "max-height")
        .expect("the sheet bounds that window, or the rows grow it without limit");
    assert!(
        bound.contains("vh"),
        "and the bound gives way to the viewport, or a short one leaves the box off screen: {bound}",
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

    // No emoji list: the set is the typeahead's own and this crate reads
    // nothing of it, so a `:sm` draft opens nothing here.
    for (draft, mark) in [("/m", "#i-cmd"), ("&cli", "#i-bot")] {
        let (_status, page) = composer(&config, draft).await;
        assert!(page.contains(&format!("href=\"{mark}\"")), "{draft} draws {mark}: {page}");
    }

    // The file list needs a seat whose tree has something in it.
    let (_status, page) = composer(&config, "@home").await;
    assert!(page.contains("class=\"ac\""), "a file query opens the file list: {page}");
    assert!(page.contains("href=\"#i-file\""), "which draws the file mark: {page}");
}

/// One file is gitignored in the fleet's project tree, and one is not.
fn seed_ignored_tree(dir: &tempfile::TempDir) {
    let root = dir.path().join("forge");
    std::fs::create_dir_all(root.join(".git")).expect("mkdir");
    std::fs::write(root.join(".gitignore"), "ignored.rs\n").expect("write");
    std::fs::write(root.join("ignored.rs"), "").expect("write");
    std::fs::write(root.join("visible.rs"), "").expect("write");
}

/// The walk runs with the user's own gitignore preference, so a second view
/// answers as the terminal does for one tree: with `respectGitignore` off,
/// the file the tree's own `.gitignore` names is offered.
#[tokio::test]
async fn the_file_list_follows_the_ignore_preference() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    seed_ignored_tree(&dir);
    fleet.set_user_preferences(serde_json::json!({ "respectGitignore": false }));
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "@ignored").await;

    assert!(
        page.contains("<em>ignored</em>.rs"),
        "an ignored file is offered once the preference is off: {page}",
    );
}

/// The same tree with the preference unset offers it not at all: an absent
/// key reads as the CLI's own default, which is to respect the file. The
/// walk is then asked for the other file, because a list that came back
/// empty for any other reason reads the same as a hidden one.
#[tokio::test]
async fn the_file_list_hides_ignored_files_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    seed_ignored_tree(&dir);
    fleet.set_user_preferences(serde_json::json!({}));
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "@ignored").await;
    assert!(!page.contains("class=\"ac\""), "no list opens for a file the tree ignores: {page}");

    let (_status, page) = composer(&config, "@visible").await;
    assert!(
        page.contains("<em>visible</em>.rs"),
        "and the same tree is walked for a file it does not ignore: {page}",
    );
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
    fleet.advertise(&SessionSlot::lead("Busytools", "forge"), commands.clone(), Vec::new());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "/cmd").await;

    assert_eq!(
        page.matches("class=\"it sel\"").count() + page.matches("class=\"it\"").count(),
        40,
        "every candidate reaches the window, which is what scrolls: {page}",
    );
    // The list is forge's table plus the advertised commands it does not
    // shadow, which is what the header counts.
    let listed = forge_server::commands::FORGE_COMMANDS.len() + commands.len();
    assert!(page.contains(&format!(">{listed}<")), "and the header counts them: {page}");
}

/// A live take's controls submit and abandon it. The mic is the one that
/// submits - the TUI's own rule, where the key that opens a take closes it -
/// and the row's cancel abandons. Catches a take that can only be abandoned,
/// which leaves the landed words and the landed beat unreachable from the
/// page however well the route serves them.
#[tokio::test]
async fn a_live_takes_controls_submit_and_abandon_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(started(1));
    settle().await;

    let (_status, page) = composer(&config, "a draft while the take runs").await;

    assert!(
        page.contains(r#"hx-vals="{&quot;action&quot;:&quot;stop&quot;}""#),
        "the mic submits the take it started: {page}",
    );
    assert!(
        page.contains(r#"hx-vals="{&quot;action&quot;:&quot;cancel&quot;}""#),
        "and the row's cancel abandons it: {page}",
    );
    assert!(page.contains("/session/Busytools/forge/lead/dictate"), "{page}");
    assert!(
        !page.contains("stopping a take is not available yet"),
        "and neither carries a refusal: {page}",
    );
}

/// The URL a control posts to is a URL a route serves, and nothing but this
/// reads one side and uses it. A markup assertion pins the string it finds
/// and a dispatch test posts to the route table, so both stay green while
/// the click does nothing at all: htmx does not swap a 404.
#[tokio::test]
async fn every_control_posts_to_a_url_a_route_serves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Permission);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::PermissionRequest {
        key: lead(),
        tool_id: "test-tool".to_owned(),
        request: permission(),
    });
    settle().await;

    // Both faces of the composer: the dock, and the box with a take running.
    let (_status, dock) = composer(&config, "").await;
    fleet.emit(started(1));
    settle().await;
    let (_status, typed) = composer(&config, "a draft").await;

    let mut posted = 0;
    for region in [&dock, &typed] {
        for url in region.split("hx-post=\"").skip(1).filter_map(|rest| rest.split('"').next()) {
            posted += 1;
            let (status, _body) = post(&config, url, "draft=anything").await;
            assert_ne!(
                status,
                reqwest::StatusCode::NOT_FOUND,
                "{url} is a URL no route serves, so the control does nothing: {region}",
            );
        }
    }
    assert!(posted >= 2, "the composer posts somewhere in both of its faces");
}

/// The dock's rows are live choices again, which is what the marker means:
/// the first option is the one a key would take, and choosing it answers.
#[tokio::test]
async fn a_docks_rows_read_as_live_choices() {
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

    assert!(page.contains("class=\"opt sel\""), "the first row is marked: {page}");
    // A space before it: the demoted marker is a class among others
    // (`class="opt sel off"`), and the field's own `autocomplete="off"`
    // ends in the word without the space.
    assert!(!page.contains(" off\""), "with nothing demoted: {page}");
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

// ---------- the composer: the write half ----------

/// Posting a form to a session's route, the way a control does.
async fn post(config: &WebConfig, path: &str, body: &str) -> (reqwest::StatusCode, String) {
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}{path}", config.port))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body.to_owned())
        .send()
        .await
        .expect("served");
    let status = response.status();
    (status, response.text().await.expect("the body reads"))
}

/// A draft sent from the box reaches the core as a prompt for that seat,
/// which is the whole of what the send button is for.
#[tokio::test]
async fn the_send_reaches_the_core() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.intercept_dispatch();
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (status, _body) = post(
        &config,
        "/session/Busytools/forge/lead/send",
        "draft=push%20it%20once%20CI%20is%20green",
    )
    .await;

    assert_eq!(status, reqwest::StatusCode::OK, "a send the core accepts answers with the box");
    let dispatched = fleet.dispatched();
    assert_eq!(dispatched.len(), 1, "exactly one command: {dispatched:?}");
    let forge_server::Command::Prompt { key, text, .. } = &dispatched[0] else {
        panic!("a send is a prompt: {:?}", dispatched[0]);
    };
    assert_eq!(key, &lead(), "addressed to the seat the composer belongs to");
    assert_eq!(text, "push it once CI is green", "and carrying what was typed");
}

/// The box comes back empty, because the draft it held has gone to the
/// core: a page that kept it would send the same message twice.
#[tokio::test]
async fn a_sent_draft_leaves_the_box_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.intercept_dispatch();
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, body) = post(&config, "/session/Busytools/forge/lead/send", "draft=hello").await;

    assert!(body.contains("id=\"draft\""), "the region is the box it swaps in: {body}");
    assert!(
        body.contains("></textarea>"),
        "and the box is empty, so the words are not sent twice: {body}",
    );
}

/// The dock's option answers the prompt it was drawn for, with the action
/// the core built rather than one the browser named: the option's own
/// meaning is what the CLI decides on, and a browser that could send any
/// action could allow what the prompt never offered.
#[tokio::test]
async fn answering_the_dock_reaches_the_core() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.intercept_dispatch();
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Permission);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::PermissionRequest {
        key: lead(),
        tool_id: "tu-1".to_owned(),
        request: permission(),
    });
    settle().await;

    let (status, _body) =
        post(&config, "/session/Busytools/forge/lead/answer", "tool_id=tu-1&option_id=edits").await;

    assert_eq!(status, reqwest::StatusCode::OK, "the answer is accepted");
    let dispatched = fleet.dispatched();
    assert_eq!(dispatched.len(), 1, "exactly one command: {dispatched:?}");
    let forge_server::Command::RespondPermission { key, tool_id, outcome } = &dispatched[0] else {
        panic!("an answer is a permission response: {:?}", dispatched[0]);
    };
    assert_eq!(key, &lead());
    assert_eq!(tool_id, "tu-1", "addressed to the prompt that asked");
    let forge_primitives::permission_interaction::PermissionOutcome::Selected {
        option_id,
        action,
        ..
    } = outcome
    else {
        panic!("a chosen option is a selection: {outcome:?}");
    };
    assert_eq!(option_id, "edits");
    assert_eq!(
        action,
        &forge_primitives::permission_interaction::PermissionAction::AllowWithInput,
        "with the action the core built for that option, not one the browser sent",
    );
}

/// A send from a seat nothing is running behind is answered with the box the
/// core would draw for it, which says why, and the field is left alone: the
/// words never reached the core, so nothing about them went anywhere and a
/// box that replaced the field would destroy them.
#[tokio::test]
async fn a_send_with_no_session_answers_with_the_seat() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    // No intercept: the command has to reach the core for the core to
    // refuse it, which is the thing under test.
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let seat = SessionSlot::lead("Personal", "dotfiles");
    let holds_a_session = || fleet.surface().agents().all().iter().any(|row| row.slot == seat);
    assert!(!holds_a_session(), "precondition: nothing is behind this seat");

    let (status, body) =
        post(&config, "/session/Personal/dotfiles/lead/send", "draft=anyone").await;

    assert_eq!(status, reqwest::StatusCode::OK, "the click swaps the box: {body}");
    assert!(body.contains("class=\"blocked\""), "which is the reason it takes no input: {body}");
    assert!(body.contains("not running"), "and says what that reason is: {body}");
    assert!(
        body.contains("id=\"draft\"") && body.contains("hx-preserve"),
        "with the reader's field kept, because their words never reached the core: {body}",
    );
    assert!(!holds_a_session(), "with nothing created for it to deliver later");
}

/// The dictation controls start and stop a take, which is the only thing
/// they ever claimed to do.
#[tokio::test]
async fn the_dictation_controls_reach_the_core() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.intercept_dispatch();
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    for action in ["start", "stop", "cancel"] {
        let (status, _) =
            post(&config, "/session/Busytools/forge/lead/dictate", &format!("action={action}"))
                .await;
        assert_eq!(status, reqwest::StatusCode::OK, "{action} is accepted");
    }

    let dispatched = fleet.dispatched();
    assert_eq!(dispatched.len(), 3, "a take started, submitted and abandoned: {dispatched:?}");
    assert!(
        matches!(&dispatched[0], forge_server::Command::DictateStart { key } if key == &lead()),
        "{:?}",
        dispatched[0],
    );
    assert!(
        matches!(&dispatched[1], forge_server::Command::DictateStop { submit: true, .. }),
        "stopping submits the take: {:?}",
        dispatched[1],
    );
    assert!(
        matches!(&dispatched[2], forge_server::Command::DictateStop { submit: false, .. }),
        "cancelling abandons it: {:?}",
        dispatched[2],
    );
}

/// Every control that can act does what it says, so none of them carries
/// the refusal it used to: a reason on a control that can act is a lie
/// about the control. The last one went with the pick, which the page's own
/// keys now write, so nothing in the composer refuses anything.
#[tokio::test]
async fn nothing_that_can_act_is_drawn_unavailable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.advertise(
        &SessionSlot::lead("Busytools", "forge"),
        vec![forge_primitives::AvailableCommand::new("model", "Switch model")],
        Vec::new(),
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, list_page) = composer(&config, "/m").await;
    assert!(
        list_page.contains("class=\"ac\"") && list_page.contains("class=\"keys\""),
        "the list draws, and names the keys that choose a row: {list_page}",
    );
    assert!(!list_page.contains("not available yet"), "with nothing refusing: {list_page}");

    fleet.seed_test_pending_interaction(&lead(), PendingKind::Permission);
    fleet.emit(SessionUpdate::PermissionRequest {
        key: lead(),
        tool_id: "tu-1".to_owned(),
        request: permission(),
    });
    settle().await;

    let (_status, box_page) = composer(&config, "a draft").await;
    let (_status, dock_page) = composer(&config, "").await;

    for page in [&box_page, &dock_page] {
        assert!(!page.contains("not available yet"), "no control that can act refuses: {page}");
        assert!(!page.contains("disabled"), "and none is drawn unavailable: {page}");
    }
    assert!(dock_page.contains("hx-post"), "the dock's options post an answer: {dock_page}");
}

/// Answering clears the dock on every view, not only the one that clicked:
/// the update says the prompt is gone, and a page that kept drawing it
/// would offer an answer to a question already settled.
#[tokio::test]
async fn the_dock_clears_when_the_prompt_is_gone() {
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
    assert!(page.contains("class=\"dock\""), "precondition: the dock is drawn: {page}");

    // Both halves of an answer, as the core does them: the pending set
    // lets go, and the stream says so.
    fleet.clear_test_pending(&lead());
    fleet.emit(SessionUpdate::PendingInteractionResolved {
        key: lead(),
        tool_id: "tu-1".to_owned(),
    });
    settle().await;

    let (_status, page) = composer(&config, "").await;

    assert!(!page.contains("class=\"dock\""), "the prompt is gone, so the box is back: {page}");
    assert!(page.contains("id=\"draft\""), "and it takes input again: {page}");
}

fn mcp_server(name: &str, scope: &str, tools: usize) -> McpServerStatus {
    let tool_list: Vec<serde_json::Value> =
        (0..tools).map(|i| serde_json::json!({ "name": format!("tool-{i}") })).collect();
    let mut server = serde_json::from_value::<McpServerStatus>(serde_json::json!({
        "name": name,
        "status": "connected",
        "tools": tool_list,
    }))
    .expect("an MCP server status");
    server.scope = Some(scope.to_owned());
    server
}

fn process(pid: u32, parent_pid: u32, name: &str, command: &str, memory_mb: u64) -> ProcessEntry {
    ProcessEntry {
        pid,
        parent_pid,
        name: name.to_owned(),
        command: command.to_owned(),
        memory_bytes: memory_mb * 1024 * 1024,
    }
}

/// A prompt the core still holds answers from the core's own copy, which is
/// the case the retention exists for: the stream carried the request once
/// and kept it nowhere, so a view that attached late has only what the core
/// kept beside the answer's oneshot. Catches a render that draws options an
/// answer cannot reach, where the click looks like nothing happened.
#[tokio::test]
async fn the_prompt_the_core_kept_is_answered_from_the_core() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.intercept_dispatch();
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Question);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    // Nothing is emitted: this view attached after the prompt landed.
    let (status, region) = composer(&config, "").await;
    assert!(status.is_success(), "the dock draws from the core's copy: {region}");

    let (status, _body) = post(
        &config,
        "/session/Busytools/forge/lead/answer",
        "tool_id=test-tool&option_id=staging",
    )
    .await;

    assert_eq!(status, reqwest::StatusCode::OK, "and the option it drew answers");
    let dispatched = fleet.dispatched();
    assert_eq!(dispatched.len(), 1, "exactly one command: {dispatched:?}");
    let forge_server::Command::RespondQuestion { tool_id, outcome, .. } = &dispatched[0] else {
        panic!("an answer is a question response: {:?}", dispatched[0]);
    };
    assert_eq!(tool_id, "test-tool", "addressed to the prompt the core holds");
    let forge_primitives::QuestionOutcome::Answered { selected_option_ids, .. } = outcome else {
        panic!("a chosen option is an answer: {outcome:?}");
    };
    assert_eq!(selected_option_ids, &vec!["staging".to_owned()]);
}

/// A prompt the core has let go answers with the box rather than with a
/// refusal nothing swaps: htmx does not swap a 409, so a stale dock would
/// keep drawing and the click would do nothing, repeatably.
#[tokio::test]
async fn a_prompt_the_core_let_go_answers_with_the_box() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.intercept_dispatch();
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Permission);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::PermissionRequest {
        key: lead(),
        tool_id: "test-tool".to_owned(),
        request: permission(),
    });
    settle().await;
    fleet.clear_test_pending(&lead());

    let (status, body) =
        post(&config, "/session/Busytools/forge/lead/answer", "tool_id=test-tool&option_id=once")
            .await;

    assert_eq!(status, reqwest::StatusCode::OK, "the click swaps something: {body}");
    assert!(body.contains("id=\"draft\""), "and what it swaps in is the box: {body}");
    assert!(fleet.dispatched().is_empty(), "with nothing dispatched for a prompt that is gone");
}

/// A send with nothing in it is refused rather than dispatched, and the
/// refusal is one htmx swaps: a 400 leaves the click looking like nothing
/// happened, and a field with only spaces in it is not a message.
#[tokio::test]
async fn an_empty_send_is_refused_with_the_box() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.intercept_dispatch();
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    for body in ["draft=", "draft=%20%20"] {
        let (status, region) = post(&config, "/session/Busytools/forge/lead/send", body).await;

        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "the click swaps the box for {body:?}: {region}"
        );
        assert!(region.contains("id=\"draft\""), "and what it swaps in is the box: {region}");
        assert!(
            !region.contains("class=\"send\""),
            "with no send control for a field nothing can be sent from: {region}",
        );
        assert!(fleet.dispatched().is_empty(), "with nothing dispatched: {region}");
    }
}

/// A page opened on a seat that cannot take input draws the composer outside
/// the region the stream swaps, on an event of its own. Catches the composer
/// being folded back into the columns' morph, which is what wiped a draft
/// the reader had typed and left the dock drawing only on a keystroke.
#[tokio::test]
async fn the_composer_has_its_own_event_and_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _ct, page) = get(&config, "/session/Busytools/forge/lead").await;

    let region = page.find("id=\"session-body\"").expect("the region the columns' event swaps");
    let slot = page.find("id=\"composer-slot\"").expect("the composer's own wrapper");
    let target = page.find("id=\"comp\"").expect("the box the composer's event swaps");
    assert!(
        region < slot && slot < target,
        "the composer sits beside the region, not inside it: {page}",
    );
    assert!(
        page.contains("sse-swap=\"composer\"") && page.contains("hx-target=\"#comp\""),
        "with a listener of its own aimed at the box: {page}",
    );
    let region_markup = &page[region..slot];
    assert!(
        !region_markup.contains("sse-swap=\"composer\""),
        "and no composer listener inside the region a morph replaces: {region_markup:.200}",
    );
}

/// The region of the `nth` composer event the stream sends, or `None` when
/// it does not arrive within five seconds.
async fn nth_composer_event(response: reqwest::Response, nth: usize) -> Option<String> {
    nth_composer_event_within(response, nth, std::time::Duration::from_secs(5)).await
}

/// The same, for a caller waiting on something slower than an update - the
/// stream's own tick, which is armed one interval out.
async fn nth_composer_event_within(
    response: reqwest::Response,
    nth: usize,
    within: std::time::Duration,
) -> Option<String> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut seen = String::new();
    // The deadline is the caller's, not each read's: a stream that keeps
    // sending something else - a tick, another seat's news - would otherwise
    // keep the wait alive for as long as it keeps talking.
    let deadline = std::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        let Ok(Some(Ok(chunk))) = tokio::time::timeout(left, stream.next()).await else {
            return None;
        };
        seen.push_str(&String::from_utf8_lossy(&chunk));
        if seen.matches("event: composer").count() >= nth {
            return seen.rsplit("event: composer").next().map(str::to_owned);
        }
    }
}

/// A prompt settled in the core redraws the box on a view that never held
/// the request: its dock draws from the core's record of what is pending,
/// so this view's copy of the prompt is not what decides. Catches a fold
/// that answers only for the copy it happens to hold, which leaves the dock
/// drawing over a prompt the core has let go.
#[tokio::test]
async fn a_prompt_settled_elsewhere_redraws_the_composer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(SessionUpdate::PendingInteractionResolved {
        key: lead(),
        tool_id: "a-prompt-this-view-never-folded".to_owned(),
    });

    let region = nth_composer_event(stream, 1).await.expect("the box redraws");
    assert!(region.contains("id=\"comp\""), "and what it draws is the box: {region}");
}

/// An option the prompt never offered is not an answer. The outcome is
/// built from the core's own list, so a browser naming an id of its own
/// neither allows what the prompt never offered nor rejects the call by
/// inventing one. Catches an arm that forwards whatever arrives.
#[tokio::test]
async fn an_option_the_prompt_never_offered_is_not_an_answer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.intercept_dispatch();
    fleet.seed_test_pending_interaction(&lead(), PendingKind::Question);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    for (tool_id, option_id, what) in [
        ("test-tool", "made-up", "an option the question never offered"),
        ("not-even-the-tool", "staging", "a call the seat is not holding"),
    ] {
        let (status, body) = post(
            &config,
            "/session/Busytools/forge/lead/answer",
            &format!("tool_id={tool_id}&option_id={option_id}"),
        )
        .await;

        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "the click swaps something rather than a refusal nothing draws ({what}): {body}",
        );
        assert!(
            fleet.dispatched().is_empty(),
            "and nothing is dispatched for {what}, which the core does not hold",
        );
    }
}

// ---------- how a row gives way: the schedules section and a command row
// (#1228, #1229) ----------

/// The scope a schedule row's own rule is written against, which is the
/// section the page puts those rows in.
const SCHEDULES_SCOPE: &str = ".sec[data-k=\"sec-schedules\"]";

/// A schedule row inverts the shared row rule: the next-fire is what the row
/// is for and the label is its context, so the label is the half that gives
/// way and the value keeps its width.
///
/// Read off the served sheet, because the sheet is what a browser measures.
/// A value left shrinkable is what squeezed a next-fire to nothing once a
/// label passed forty-eight characters; a value that cannot give way at all
/// is what ran 34.8px past the row and 4.8px past the rail, with its own tail
/// cut and no ellipsis. Both halves are pinned here because the fix for one
/// is the other's failure.
#[tokio::test]
async fn the_schedule_row_gives_up_its_label_and_keeps_its_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    let (_status, _content_type, sheet) = get(&config, "/web.css").await;

    let value = blocks_for(&sheet, &format!("{SCHEDULES_SCOPE} .sb .kv .v"));
    assert!(
        value.iter().any(|block| block.contains("flex: 0 0 auto")),
        "the next-fire keeps its own width instead of shrinking: {value:?}",
    );
    assert!(
        value.iter().any(|block| block.contains("max-width: calc(70% - 12px)")),
        "bounded by what the label's floor leaves it, so it cannot run past the rail: {value:?}",
    );
    let label = blocks_for(&sheet, &format!("{SCHEDULES_SCOPE} .sb .kv .k"));
    assert!(
        label
            .iter()
            .any(|block| block.contains("min-width: 30%") && block.contains("flex: 1 1 auto")),
        "and the label is the half that shortens, with a floor so it cannot vanish: {label:?}",
    );
    let row = shared_blocks(&sheet, ".kv", "display: flex");
    assert_eq!(row.len(), 1, "one shared row rule, left as it was");
    assert!(
        row[0].contains("gap: 12px"),
        "and its gap is the 12px the value's cap subtracts, so retuning one means retuning the other: {row:?}",
    );
    // Counted rather than read off the shared block: the scoped selector ends
    // in the shared one's text, so one list holds both, and a second copy
    // written at the shared selector is the way this reaches every section.
    let rules = blocks_for(&sheet, ".sb .kv .v");
    let inverted: Vec<&String> =
        rules.iter().filter(|block| block.contains("flex: 0 0 auto")).collect();
    assert_eq!(inverted.len(), 1, "one rule carries the inversion: {inverted:?}");
    assert!(
        inverted[0].contains("max-width: calc(70% - 12px)"),
        "and it is the scoped rule, not a copy written at the shared selector: {inverted:?}",
    );
}

/// The blocks a selector is written in that begin with `first`. [`blocks_for`]
/// matches substrings, and a scoped selector here ends in the shared one's
/// text, so the shared block is the one carrying the shared declaration rather
/// than whichever the sheet writes last.
fn shared_blocks(css: &str, selector: &str, first: &str) -> Vec<String> {
    blocks_for(css, selector).into_iter().filter(|block| block.starts_with(first)).collect()
}

/// The scope that rule is written against is the one the page puts a schedule
/// row inside, so the inversion reaches the rows rather than sitting in the
/// sheet matching nothing.
#[tokio::test]
async fn the_schedule_rows_sit_inside_the_scope_the_sheet_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(dir.path(), &[("Busytools", &["forge"])]).expect("the fleet builds");
    fleet.start("Busytools", "forge").expect("forge is declared");
    fleet.add_cron("forge", "stand-up").expect("forge is declared");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    let scope = page
        .find("data-k=\"sec-schedules\"")
        .expect("the schedules section carries the scope the sheet's rule names");
    let body = &page[scope..];
    let end = body.find("</details>").expect("and the section closes");
    let rows = &body[..end];
    assert!(
        rows.contains("<div class=\"kv\">"),
        "a row, not just a heading, sits inside it: {rows}"
    );
    assert!(rows.contains("stand-up"), "with the project's own cron among them: {rows}");
}

/// A command with no description is named by its own command in the row
/// title, and the arguments at the end are what tell one invocation from
/// another, so that title wraps rather than running off one line and
/// ellipsising. It stops at three lines, and the body draws the whole
/// command, because a capped title cannot reach its own tail.
#[tokio::test]
async fn a_command_row_that_is_named_by_its_command_wraps_within_a_cap() {
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
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"find /tmp/rev40.R9A5 -depth -delete; find /tmp/rev41.zUeo -depth -delete; rm -f /tmp/gamarr-rev-dirs.txt; ls -d /tmp/rev40.R9A5 /tmp/rev41.zUeo 2>&1; echo done"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("<details class=\"leaf cmd\""),
        "the row says its title is the command it ran: {page}",
    );
    assert!(
        page.contains("<span class=\"pfx\">$</span> find /tmp/rev40.R9A5 -depth -delete"),
        "and the body draws that command, where the title's own clamp cannot reach: {page}",
    );
    let (_status, _content_type, sheet) = get(&config, "/web.css").await;
    let title = blocks_for(&sheet, "details.leaf.cmd > summary .tn");
    assert!(
        title.iter().any(|block| block.contains("white-space: normal")),
        "so that title wraps instead of running off the one line: {title:?}",
    );
    assert!(
        title.iter().any(|block| block.contains("-webkit-line-clamp: 3")),
        "and stops at the cap a real command was measured against: {title:?}",
    );
    assert!(
        !sheet.contains("details.leaf.cmd > summary {"),
        "with nothing else about the row moved to do it",
    );
    let rules = blocks_for(&sheet, "details.leaf > summary .tn");
    assert!(
        rules.iter().any(|block| block.contains("white-space: nowrap")),
        "and every other call's row still ellipsises on one line",
    );
    // No count here, unlike the schedule rule: this sheet writes the scoped
    // selector as `details.leaf.cmd > summary .tn`, which does not contain the
    // shared selector's text, so this list holds every block that could put
    // the wrap on every call and one of them carrying it is the failure.
    assert!(
        !rules.iter().any(|block| block.contains("-webkit-line-clamp")),
        "and the wrap not written at the shared selector, where it would reach every call: {rules:?}",
    );
}

/// A command that carries a description is named by that description, which
/// is a sentence rather than a command line, so its row keeps the one-line
/// title every other call has.
#[tokio::test]
async fn a_command_with_a_description_keeps_the_one_line_title() {
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
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"just check","description":"run the gates"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"    Finished in 41.2s"}]}}"#,
            ],
        )
        .expect("the transcript is written");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    assert!(
        page.contains("run the gates"),
        "the described command's row is there to be seen at all: {page}",
    );
    assert!(
        !page.contains("leaf cmd"),
        "a described command's title is a sentence, and it keeps its one line: {page}",
    );
}

/// The page puts a `display: contents` wrapper between the app and the three
/// children it lays out, so a rule that hops over that wrapper with a child
/// combinator matches nothing - and a placement rule that matches nothing
/// leaves its child in an implicit column that takes the width from the ones
/// beside it. Catches the hop whichever side of the sheet it is written on.
#[tokio::test]
async fn the_apps_children_are_reached_through_the_wrapper() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;
    for child in ["aside class=\"rail left\"", "main class=\"chat\""] {
        assert!(
            page.find(child).expect("the child is on the page")
                > page.find("id=\"session-body\"").expect("the wrapper"),
            "{child} sits inside the wrapper, not beside it: {page}",
        );
    }

    let (_status, _content_type, sheet) = get(&config, "/web.css").await;
    // Read rather than matched as one string: `.app>.rail` and `.app >main.chat`
    // are the same hop, and a guard that only knows the spaced spelling is the
    // one a later hand slips past.
    let mut rest = sheet.as_str();
    while let Some(at) = rest.find(".app") {
        let tail = &rest[at + 4..];
        if tail.trim_start().starts_with('>') {
            let rule = rest[at..].split(['{', ',']).next().unwrap_or_default();
            panic!("{rule} hops over the wrapper, which no child of the app is behind");
        }
        rest = tail;
    }
    for child in [".rail.left", "main.chat", ".rail.right"] {
        assert!(
            sheet.contains(&format!(".app {child}")),
            "and {child} is placed through the wrapper, which is the hop its own rule has",
        );
    }
    let narrow = sheet.find("@media (max-width: 980px)").expect("the narrow block");
    assert!(
        sheet[narrow..].contains(".app main.chat"),
        "with the narrow column named the same way, or the placement rule wins by source order",
    );
}

/// A push draws the box as the reader's own text has earned it: the server
/// does not hold the field's text, and the only way to send is the control
/// that text draws, so a pushed region that dropped it would take the send
/// away for as long as the reader stopped typing.
#[tokio::test]
async fn a_push_keeps_the_control_the_draft_earned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(SessionUpdate::AuthRequired {
        key: lead(),
        method_name: "oauth".to_owned(),
        method_description: "Sign in with your browser".to_owned(),
    });

    let region = nth_composer_event(stream, 1).await.expect("the push draws the box");
    assert!(region.contains("class=\"box\""), "and what it draws is the box: {region}");
    assert!(
        region.contains("class=\"send\""),
        "with the send control the field's text earned kept drawn: {region}",
    );
    assert!(region.contains("hx-preserve"), "and the field itself kept: {region}");
}

/// A message appended to the conversation is not news about the box: the box
/// holds none of the conversation, and pushing it would redraw the field and
/// the send control from a draft the server cannot see. Catches the box
/// riding the columns' event, which takes the send control away within a
/// second of the reader stopping typing.
#[tokio::test]
async fn a_message_does_not_push_the_composer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(SessionUpdate::ChatAppended {
        key: lead(),
        msg: user_frame("composer-quiet-1", "a message the box has no use for"),
    });

    assert!(
        !composer_event_within(stream, std::time::Duration::from_secs(2)).await,
        "a message redraws the columns and leaves the box alone",
    );
}

/// The dock carries the reader's field. A push cannot carry their text - the
/// browser holds it - and a node the pushed markup does not have is removed,
/// so the dock renders the field itself for the browser to keep. Without it
/// the reader's words go with the box the moment a prompt arrives.
#[tokio::test]
async fn the_dock_keeps_the_readers_field() {
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

    let dock = page.find("class=\"dock\"").expect("precondition: the dock draws");
    let field = page.find("id=\"draft\"").expect("the dock carries the field the box had");
    assert!(field > dock, "inside the dock, so a morph of it keeps the element: {page}");
    assert!(
        page[field..].contains("hx-preserve"),
        "with the marker that makes the browser keep its own text: {page}",
    );
    assert_the_field_is_complete(&page);

    let (_status, _content_type, sheet) = get(&config, "/web.css").await;
    assert!(
        sheet.contains(".dock #draft"),
        "and the sheet never draws it: the field belongs to the box",
    );
}

/// The field as every render draws it. Both the live box and the parked one
/// have to be the same complete field, because which node ends up in the box
/// is the browser's business: a page whose first render is a parked one - a
/// seat that has not started, or one already holding a prompt - hands the
/// parked node to the box, and a field with no name sends nothing.
fn assert_the_field_is_complete(region: &str) {
    let at = region.find("id=\"draft\"").expect("a field");
    let opens = region[..at].rfind('<').expect("the tag that opens it");
    let tag = &region[opens..];
    let tag = &tag[..tag.find('>').expect("the tag that closes it")];
    for attr in [
        "name=\"draft\"",
        "rows=\"3\"",
        "autocomplete=\"off\"",
        "spellcheck=\"false\"",
        "placeholder=\"Type a message\u{2026}\"",
    ] {
        assert!(tag.contains(attr), "the field is missing {attr}: {tag}");
    }
}

/// Whether a composer event arrives within `within`: a stream that should
/// stay quiet is as much a property as one that speaks, and the tick's own
/// columns event is not a composer one.
async fn composer_event_within(response: reqwest::Response, within: std::time::Duration) -> bool {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut seen = String::new();
    let deadline = std::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        let Ok(Some(Ok(chunk))) = tokio::time::timeout(left, stream.next()).await else {
            return false;
        };
        seen.push_str(&String::from_utf8_lossy(&chunk));
        if seen.contains("event: composer") {
            return true;
        }
    }
}

/// The send's own answer is the one render that replaces the field, and the
/// reader's own render keeps it. The difference is the whole of the draft's
/// survival, and nothing else in the suite reads it.
#[tokio::test]
async fn only_the_sends_answer_replaces_the_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.intercept_dispatch();
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, typed) = composer(&config, "half a draft").await;
    assert!(typed.contains("hx-preserve"), "the reader's own render keeps the field: {typed}");
    assert!(typed.contains("half a draft"), "with their words in it: {typed}");
    assert_the_field_is_complete(&typed);

    let (status, sent) = post(&config, "/session/Busytools/forge/lead/send", "draft=sent").await;
    assert_eq!(status, reqwest::StatusCode::OK, "the send answers with the box: {sent}");
    assert!(
        !sent.contains("hx-preserve"),
        "and its answer is the one render that takes the browser's field: {sent}",
    );
    assert!(
        sent.contains("placeholder=\"Type a message\u{2026}\"></textarea>"),
        "drawn empty, because the words went to the core: {sent}",
    );
}

/// A blocked box carries the field too. It draws the reason instead of the
/// box, and a push rendered as blocked would otherwise leave the page with no
/// field at all: the reader's words go with the element, because a preserved
/// node the new markup does not have is removed. A compacting seat reaches
/// this on its own, with no update the reader caused.
#[tokio::test]
async fn a_blocked_push_keeps_the_readers_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(SessionUpdate::ChatAppended {
        key: lead(),
        msg: forge_primitives::Message::System {
            subtype: "status".to_owned(),
            session_id: None,
            data: serde_json::json!({"status": "compacting"}),
        },
    });

    let region = nth_composer_event(stream, 1).await.expect("the push redraws the box");
    assert!(
        region.contains("class=\"blocked\""),
        "the box says why it is not taking input: {region}"
    );
    let blocked = region.find("class=\"blocked\"").expect("the blocked element");
    let field = region.find("id=\"draft\"").expect("and still carries the reader's field");
    assert!(field > blocked, "inside it, which is the nesting its own rule hides: {region}");
    assert!(
        region[field..].contains("hx-preserve"),
        "marked so the browser keeps its own: {region}",
    );
    assert_the_field_is_complete(&region);

    let (_status, _content_type, sheet) = get(&config, "/web.css").await;
    assert!(
        sheet.contains(".blocked #draft"),
        "and the sheet never draws it: the field belongs to the box",
    );
}

/// What the reader types into a question's notes is theirs, and a push cannot
/// carry it, so the field is kept the way the box's own is. The id names the
/// question rather than the field: words written for one question are not an
/// answer to the next, and a fresh id is a fresh field.
#[tokio::test]
async fn the_notes_field_belongs_to_its_question() {
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
    assert!(
        page.contains("id=\"notes-tu-2\""),
        "the notes field is named for the question it answers: {page}",
    );
    let notes = page.find("id=\"notes-tu-2\"").expect("the notes field");
    assert!(
        page[notes..].contains("hx-preserve"),
        "and kept across a push, like the box's own: {page}",
    );
    assert!(
        page.contains("class=\"lbl own\""),
        "and the control that submits those words is marked, because Enter in the field goes to it: {page}",
    );

    // The next question is a different field, so what was typed for this one
    // cannot come back as an answer to it.
    fleet.emit(SessionUpdate::QuestionRequest {
        key: lead(),
        tool_id: "tu-9".to_owned(),
        request: question_from("tu-9"),
    });
    settle().await;
    let (_status, next) = composer(&config, "").await;
    assert!(next.contains("id=\"notes-tu-9\""), "the next question names its own: {next}");
}

/// A take that ends is the box's news even though the fold has already
/// taken the take: the boot fold applies every update before a session
/// stream does, so a guard that answered for the state it changed would find
/// the take gone, read the outcome as a stale tail, and leave the box drawn
/// live over it until the tick. Catches that guard.
#[tokio::test]
async fn a_take_that_ends_reaches_the_box() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;
    fleet.emit(started(7));
    // The fold takes the take when this lands, so the stream's own
    // application of it finds nothing: that is the whole of the trap.
    fleet.emit(ended(
        7,
        forge_server::surface::DictateOutcome::NoAudio { peak_db: -61.0, seconds: 3 },
    ));

    let region = nth_composer_event(stream, 2).await.expect("the take's end redraws the box");
    assert!(
        region.contains("nothing above -50 dBFS in 3s"),
        "and the notice it leaves is drawn: {region}",
    );
    assert!(!region.contains("class=\"dict\""), "with the take's row gone: {region}");
}

/// The tick redraws the columns alone. A pushed box is drawn from a draft
/// the server does not have, so its region carries no list: measured on a
/// live page, a tick that redrew the composer closed an open autocomplete
/// ten seconds after the stream attached. Catches a tick that redraws the
/// box, which takes the reader's list with it.
#[tokio::test]
async fn the_tick_leaves_the_box_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let up = fleet.start("Busytools", "forge");
    assert!(up.is_ok(), "the seat is live");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let stream = open_stream_at(&config, "/session/Busytools/forge/lead/events").await;

    // Nothing is emitted: the only thing the stream has to say is its own
    // tick, which the connection arms for one interval out.
    assert!(
        !composer_event_within(stream, std::time::Duration::from_secs(13)).await,
        "the tick redraws the columns and leaves the box where it is",
    );
}

/// The dock names the keys it answers to, in the mock's own wording, and
/// names only those: a key on the line that does nothing is the same defect
/// as a control that does nothing.
#[tokio::test]
async fn the_dock_names_the_keys_it_answers_to() {
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

    let keys = page.find("class=\"keys\"").expect("the dock draws a keys line");
    let line = &page[keys..];
    let line = &line[..line.find("</div>").expect("the line closes")];
    for part in ["\u{2191}\u{2193}", "select", "\u{21b5}", "confirm", "esc", "reject"] {
        assert!(line.contains(part), "the dock names {part:?}: {line}");
    }

    fleet.emit(SessionUpdate::PermissionRequest {
        key: lead(),
        tool_id: "tu-1".to_owned(),
        request: permission(),
    });
    let question = question();
    let (_status, page) = {
        fleet.emit(SessionUpdate::QuestionRequest {
            key: lead(),
            tool_id: "tu-2".to_owned(),
            request: question,
        });
        settle().await;
        composer(&config, "").await
    };
    let keys = page.find("class=\"keys\"").expect("the question dock draws its own line");
    let line = &page[keys..];
    let line = &line[..line.find("</div>").expect("the line closes")];
    for part in ["\u{2191}\u{2193}", "move", "\u{21b5}", "submit"] {
        assert!(line.contains(part), "the question dock names {part:?}: {line}");
    }
    assert!(
        !line.contains("space") && !line.contains("toggle"),
        "and promises no toggle, because the answer sends one option: {line}",
    );
}

/// A list row carries what a pick writes into the field. The row the reader
/// sees is a drawing of the item - the sheet shortens a long path, and their
/// own query is marked inside it - so the value travels beside the drawing
/// rather than being read back out of it.
#[tokio::test]
async fn a_list_row_carries_what_a_pick_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    // A name forge does not handle itself, so the row under test is the one
    // built from the CLI's advertised list rather than from forge's table.
    fleet.advertise(
        &SessionSlot::lead("Busytools", "forge"),
        vec![forge_primitives::AvailableCommand::new("plan", "Draft a plan")],
        Vec::new(),
    );
    let project = dir.path().join("forge");
    std::fs::create_dir_all(project.join("crates/forge-web/src")).expect("mkdir");
    std::fs::write(project.join("crates/forge-web/src/home.rs"), "").expect("write");
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "/pl").await;
    assert!(
        page.contains("data-ins=\"/plan\""),
        "the command's row carries the command a pick writes: {page}",
    );

    let (_status, page) = composer(&config, "@home").await;
    assert!(
        page.contains("data-ins=\"@crates/forge-web/src/home.rs\""),
        "and a file's row carries the mention, whole, rather than the shortened drawing: {page}",
    );
}

/// The list names the keys that choose a row, and the refusal it used to
/// carry is gone with the refusal: a row that can be picked under a line
/// saying it cannot is the same defect in text as a control that does
/// nothing.
#[tokio::test]
async fn the_list_names_the_keys_that_choose_a_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    fleet.advertise(
        &SessionSlot::lead("Busytools", "forge"),
        vec![forge_primitives::AvailableCommand::new("model", "Switch model")],
        Vec::new(),
    );
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, page) = composer(&config, "/m").await;

    let keys = page.find("class=\"keys\"").expect("the list draws a keys line");
    let line = &page[keys..];
    let line = &line[..line.find("</div>").expect("the line closes")];
    for part in ["\u{2191}\u{2193}", "select", "\u{21b5}", "choose", "esc", "close"] {
        assert!(line.contains(part), "the list names {part:?}: {line}");
    }
    assert!(
        !page.contains("not available yet"),
        "and nothing in the composer refuses any more: {page}",
    );
}

/// The page ships the listener the composer's lines advertise. A drawn key
/// that nothing reads is the defect the lines exist to avoid, and the
/// listener is the only part of it a request can see: what it does with a
/// key is the browser's to show.
#[tokio::test]
async fn the_page_ships_the_keys_the_composer_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;

    let (_status, _content_type, page) = get(&config, "/session/Busytools/forge/lead").await;

    let script = page.find("addEventListener('keydown'").expect("the page reads keys");
    let script = &page[script..];
    // The mapping rather than the vocabulary: which key does what is the
    // whole of it, and two swapped arrows or an Escape bound elsewhere would
    // pass a check that only looked for the names.
    for mapping in [
        "{ ArrowDown: 1, ArrowUp: -1 }",
        "event.key === 'Enter' && !event.shiftKey",
        "opt.querySelector('.no')",
        "dataset.ins",
        "textarea.notes",
        "'.lbl.own'",
    ] {
        assert!(script.contains(mapping), "the listener maps {mapping}: {script:.400}");
    }
    assert!(
        script.contains("#comp"),
        "and it reads the composer region rather than binding to a node a swap replaces: {script:.400}",
    );
}

/// An answer whose command never reaches a session is answered with the
/// region rather than a status htmx will not swap. A dock drawn before the
/// session died is otherwise inert and stuck: its options post into nothing,
/// and its field is parked, so the reader cannot even type to force a
/// refetch.
#[tokio::test]
async fn an_answer_that_reaches_no_session_answers_with_the_region() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = fleet(dir.path());
    // Nothing is behind this seat, which is what makes the dispatch fail.
    let seat = SessionSlot::lead("Personal", "dotfiles");
    fleet.seed_test_pending_interaction(&seat, PendingKind::Question);
    let (_bound, config) = start(IpAddr::V4(Ipv4Addr::LOCALHOST), fleet.surface()).await;
    fleet.emit(SessionUpdate::QuestionRequest {
        key: seat,
        tool_id: "tu-2".to_owned(),
        request: question(),
    });
    settle().await;

    let (status, body) =
        post(&config, "/session/Personal/dotfiles/lead/answer", "tool_id=tu-2&option_id=staging")
            .await;

    assert_eq!(status, reqwest::StatusCode::OK, "the click swaps something: {body}");
    assert!(body.contains("id=\"draft\""), "and what it draws is the composer: {body}");
    assert!(
        body.contains("hx-preserve"),
        "with the reader's field kept, because nothing was answered: {body}",
    );
}
