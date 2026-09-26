//! The listener, and the routes it serves.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use forge_primitives::WebConfig;
use forge_sessions::surface::ViewSurface;
use maud::{Markup, html};

use crate::home::{Home, render};
use crate::stream::{Live, events};
use crate::work::WorkCache;
use crate::{brand, theme};

/// The stylesheet, vendored rather than read from disk: the page is served
/// from the process, and a view that needed a file beside it would be a
/// path to get wrong.
const HOME_CSS: &str = include_str!("home.css");

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
    /// What the stream has told the view: the diamonds and the banner.
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
    let wiring = Wiring { bound, state: Arc::new(state) };
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
        .route("/events", get(events))
        .route("/favicon.svg", get(favicon))
        .route("/home.css", get(home_css))
        .with_state(wiring)
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
    })
    .await
}

/// The region the stream swaps in: everything the page draws from the
/// core, and nothing it draws from the request. The page has no composer
/// and no `<details>`, so a wholesale replacement is the whole answer -
/// and a swap target that held either would be the bug, not the page.
pub(crate) async fn home_region(state: &WebState, bound: SocketAddr) -> Markup {
    render(&Home {
        surface: &state.surface,
        work: &state.work,
        live: &state.live,
        bound,
        mark: state.config.mark.as_deref(),
        theme: state.config.theme.as_deref(),
    })
    .await
}

async fn home_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], HOME_CSS)
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

/// The palette as the page's own root variables, in every page: one place
/// to change a theme, and no component carries a branch for it.
pub(crate) fn root_block(theme_name: Option<&str>) -> Markup {
    html! {
        style { ":root{" (theme::root_variables(theme_name)) "}" }
    }
}
