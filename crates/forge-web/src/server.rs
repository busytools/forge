//! The listener, and the routes it serves.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use forge_primitives::WebConfig;
use forge_sessions::surface::ViewSurface;
use maud::{Markup, PreEscaped, html};

use crate::home::{Home, render, render_region};
use crate::stream::{Live, events};
use crate::work::WorkCache;
use crate::{brand, theme};

/// The stylesheet, vendored rather than read from disk: the pages are
/// served from the process, and a view that needed a file beside it would
/// be a path to get wrong. One sheet for both pages, because a second
/// copy of a row, a mark or a palette is the defect rule 21 names.
const WEB_CSS: &str = include_str!("web.css");

/// The vendored scripts, by the name the page asks for. Each is
/// byte-for-byte as published, with its version, source and licence
/// recorded beside it in `assets/VENDOR.md`.
const SCRIPTS: &[(&str, &[u8])] = &[
    ("htmx.js", include_bytes!("../assets/htmx.min.js")),
    ("htmx-sse.js", include_bytes!("../assets/htmx-sse.min.js")),
    ("idiomorph.js", include_bytes!("../assets/idiomorph-ext.min.js")),
];

/// The vendored faces, by the name the sheet's `@font-face` asks for, with
/// the same record in `assets/VENDOR.md`. Bytes rather than `include_str!`,
/// which needs valid UTF-8 and a woff2 is not text.
const FONTS: &[(&str, &[u8])] = &[
    ("InterVariable.woff2", include_bytes!("../assets/fonts/InterVariable.woff2")),
    ("FiraCode-Regular.woff2", include_bytes!("../assets/fonts/FiraCode-Regular.woff2")),
    ("FiraCode-Medium.woff2", include_bytes!("../assets/fonts/FiraCode-Medium.woff2")),
];

/// Why the web view is not serving.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("could not bind {addr}: {source}")]
    Bind { addr: SocketAddr, source: std::io::Error },
}

/// What the view serves: the core it reads, the cache it keeps, the live
/// state its stream fills, and the config it draws with.
pub struct WebState {
    pub surface: Arc<ViewSurface>,
    pub work: Arc<WorkCache>,
    /// What the stream has told the view, which is the state no verb can
    /// answer because it is about this viewer rather than about the core.
    pub live: Mutex<Live>,
    pub config: WebConfig,
}

impl WebState {
    /// A view that has learned nothing from the stream yet.
    pub fn new(surface: Arc<ViewSurface>, work: Arc<WorkCache>, config: WebConfig) -> Self {
        Self { surface, work, live: Mutex::new(Live::new()), config }
    }
}

/// What one listener holds: the state, plus the address it came up on.
#[derive(Clone)]
pub(crate) struct Wiring {
    pub(crate) bound: SocketAddr,
    pub(crate) state: Arc<WebState>,
}

/// Bind the web view and serve it on a background task.
///
/// Returns the address it bound, or `None` when `[web] enabled` is
/// false. The caller owns the failure: the view is not a prerequisite
/// for anything, so a boot that cannot bind still boots.
pub async fn start(state: WebState) -> Result<Option<SocketAddr>, WebError> {
    if !state.config.enabled {
        return Ok(None);
    }
    let addr = SocketAddr::new(state.config.bind, state.config.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| WebError::Bind { addr, source })?;
    let bound = listener.local_addr().map_err(|source| WebError::Bind { addr, source })?;
    let state = Arc::new(state);
    // One folding subscription for the process, taken after the listener
    // is up and before anything can be served. It is a mirror: it takes no
    // backlog, so the view that renders prompts keeps the boot notice.
    tokio::spawn(crate::stream::fold(state.surface.subscribe(), Arc::clone(&state)));
    let wiring = Wiring { bound, state };
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router(wiring)).await {
            tracing::error!(
                target: "forge_web::server",
                event_name = "web_serve_failed",
                %bound,
                error = %error,
                "the web view stopped serving; restart forge to bring it back",
            );
        }
    });
    Ok(Some(bound))
}

fn router(wiring: Wiring) -> Router {
    Router::new()
        .route("/", get(home_page))
        .route("/session/{org}/{project}/{label}", get(session_page))
        .route("/session/{org}/{project}/{label}/composer", get(composer_region))
        .route("/events", get(events))
        .route("/favicon.svg", get(favicon))
        .route("/web.css", get(web_css))
        .route("/vendor/{file}", get(asset))
        .route("/fonts/{file}", get(font))
        .with_state(wiring)
}

/// The route the home's rows point at, in its three outcomes: the page for
/// a seat with something running behind it, the waking page for a seat in
/// the roster whose occupant is not up yet, and a 404 for a slot the
/// roster does not name at all. A shell would read as a session with
/// nothing in it, which is a wrong answer rather than a missing one.
async fn session_page(
    State(wiring): State<Wiring>,
    Path((org, project, label)): Path<(String, String, String)>,
) -> Response {
    match crate::session::page(&wiring.state, wiring.bound, &org, &project, &label).await {
        crate::session::Found::Page(page) => page.into_response(),
        crate::session::Found::Absent => {
            (StatusCode::NOT_FOUND, "no session slot by that name").into_response()
        }
    }
}

/// The composer on its own, which is what the box asks for while the reader
/// types. The draft comes back with the request, so a list opens against
/// what is actually in the box rather than against what the page was first
/// drawn with.
async fn composer_region(
    State(wiring): State<Wiring>,
    Path((org, project, label)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    let surface = &wiring.state.surface;
    let roster = surface.roster();
    let agents = surface.agents();
    let Some(slot) = crate::session::resolve(surface, &roster, &agents, &org, &project, &label)
    else {
        return (StatusCode::NOT_FOUND, "no session slot by that name").into_response();
    };
    let home = crate::session::context(&wiring.state, wiring.bound);
    let draft = crate::composer::draft_of(query.as_deref());
    crate::composer::render(&home, &slot, &roster, &agents, &draft).await.into_response()
}

/// One vendored script: the page's own, as published. An unknown name is a
/// 404 rather than an empty script, so a page asking for something that is
/// not vendored says so where a browser can report it.
async fn asset(Path(file): Path<String>) -> Response {
    vendored(SCRIPTS, "text/javascript; charset=utf-8", &file)
}

/// One vendored face, typed as one: a browser refuses a font handed to it
/// as JavaScript, and the two sets are looked up apart so a script asked
/// for as a font is missing rather than mislabelled.
async fn font(Path(file): Path<String>) -> Response {
    vendored(FONTS, "font/woff2", &file)
}

/// `no-cache` rather than a TTL, because a browser holding an old copy
/// would report a bug in forge's code. A validator would only turn the
/// re-fetch into a 304: there is no CDN in front of this, the files are
/// pinned, and the whole set is a little over 600KiB over loopback.
fn vendored(
    table: &'static [(&'static str, &'static [u8])],
    content_type: &'static str,
    file: &str,
) -> Response {
    let Some((_, body)) = table.iter().find(|(name, _)| *name == file) else {
        return (StatusCode::NOT_FOUND, "no such vendored asset").into_response();
    };
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], *body)
        .into_response()
}

/// The home, as the whole page.
async fn home_page(State(wiring): State<Wiring>) -> Markup {
    render(&Home {
        surface: &wiring.state.surface,
        work: &wiring.state.work,
        live: &wiring.state.live,
        bound: wiring.bound,
        mark: wiring.state.config.mark.as_deref(),
        theme: wiring.state.config.theme.as_deref(),
        font: wiring.state.config.font.as_deref(),
    })
    .await
}

/// The region the stream swaps in. The page has no composer and no
/// `<details>`, so a wholesale replacement is the whole answer - and a
/// swap target that held either would be the bug, not the page.
pub(crate) async fn home_region(state: &WebState, bound: SocketAddr) -> Markup {
    render_region(&Home {
        surface: &state.surface,
        work: &state.work,
        live: &state.live,
        bound,
        mark: state.config.mark.as_deref(),
        theme: state.config.theme.as_deref(),
        font: state.config.font.as_deref(),
    })
    .await
}

/// The stylesheet, with the same `no-cache` the scripts get: a browser
/// holding an old sheet would report a bug in forge's code, and a page
/// that looks wrong is harder to diagnose than one that reloads slowly.
async fn web_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")],
        WEB_CSS,
    )
}

/// The mark, as a standalone document a browser reads from a tab. Nothing
/// is inherited here, so the palette's accent is set on the root rather
/// than left to a cascade.
async fn favicon(State(wiring): State<Wiring>) -> impl IntoResponse {
    let body = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" color="{}">{}</svg>"#,
        theme::accent(wiring.state.config.theme.as_deref()),
        brand::mark_path(wiring.state.config.mark.as_deref()),
    );
    ([(header::CONTENT_TYPE, "image/svg+xml")], body)
}

/// The palette and the font stack as the page's own root variables, in
/// every page: one place to change a theme or a typeface, and no component
/// carries a branch for either. A font name the loader would have refused
/// contributes nothing, so the page draws in the browser's own default
/// rather than in a set nobody asked for.
pub(crate) fn root_block(theme_name: Option<&str>, font_name: Option<&str>) -> Markup {
    // Unescaped, because this is CSS: `&quot;` inside a `<style>` is
    // literal text rather than a quote, and both stacks quote a family
    // name. Every value comes from a name checked against a fixed list.
    html! {
        style {
            (PreEscaped(format!(
                ":root{{{}{}}}",
                theme::root_variables(theme_name),
                theme::font_variables(font_name).unwrap_or_default(),
            )))
        }
    }
}

/// The head every page carries: the same metadata, the same one sheet, the
/// same tab mark. Only the title differs by caller, so a second copy is how
/// the two would come to disagree about what a page loads.
pub(crate) fn page_head(title: &str, theme_name: Option<&str>, font_name: Option<&str>) -> Markup {
    html! {
        head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title { (title) }
            (root_block(theme_name, font_name))
            link rel="stylesheet" href="/web.css";
            link rel="icon" href="/favicon.svg" type="image/svg+xml";
        }
    }
}
