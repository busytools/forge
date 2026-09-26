//! The listener, and the page that proves the wiring.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::State;
use axum::routing::get;
use forge_primitives::WebConfig;
use maud::{DOCTYPE, Markup, html};

/// Why the web view is not serving.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("could not bind {addr}: {source}")]
    Bind { addr: SocketAddr, source: std::io::Error },
}

/// What the wiring page renders: the address it bound and the config it
/// read.
#[derive(Clone)]
struct Wiring {
    bound: SocketAddr,
    config: WebConfig,
}

/// Bind the web view and serve it on a background task.
///
/// Returns the address it bound, or `None` when `[web] enabled` is
/// false. The caller owns the failure: the view is not a prerequisite
/// for anything, so a boot that cannot bind still boots.
pub async fn start(config: WebConfig) -> Result<Option<SocketAddr>, WebError> {
    if !config.enabled {
        return Ok(None);
    }
    let addr = SocketAddr::new(config.bind, config.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| WebError::Bind { addr, source })?;
    let bound = listener.local_addr().map_err(|source| WebError::Bind { addr, source })?;
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router(Wiring { bound, config })).await {
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
    Router::new().route("/", get(wiring_page)).with_state(wiring)
}

/// The wiring proof, and nothing else: what the listener bound and what
/// the config asked for. The view itself replaces this page.
async fn wiring_page(State(wiring): State<Wiring>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "forge" }
            }
            body {
                h1 { "forge web view" }
                p { "listening on " (wiring.bound) }
                p {
                    "[web] enabled = " (wiring.config.enabled)
                    ", port = " (wiring.config.port)
                    ", bind = " (wiring.config.bind)
                }
            }
        }
    }
}
