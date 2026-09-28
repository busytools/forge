//! The socket a client speaks to, and what a connection needs to answer it.
//!
//! One route, `/socket`, at the address the caller bound. Every connection
//! gets its own subscription, so a second client changes nothing about the
//! first and the server sends only what a client asked for.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::routing::get;
use forge_primitives::WebConfig;
use tokio::net::TcpListener;

use crate::live::Live;
use crate::surface::ViewSurface;
use crate::work::WorkCache;

mod connection;
pub mod envelope;

/// What a connection answers from: the surface it reads and dispatches
/// through, the working-tree cache behind the git read, the live state a
/// late subscriber cannot reconstruct for itself, and the configuration the
/// greeting carries the client's half of.
pub struct TransportState {
    pub surface: Arc<ViewSurface>,
    pub work: Arc<WorkCache>,
    pub live: Mutex<Live>,
    pub config: WebConfig,
}

/// Serve the socket on `listener` until the process ends.
///
/// The listener is the caller's rather than bound here, so a test can bind
/// port 0 and know the address it bound, and so a caller that cannot bind
/// decides for itself whether that is fatal - which is what keeps a busy
/// port from becoming a new way for forge to refuse to start.
pub async fn serve(state: Arc<TransportState>, listener: TcpListener) -> anyhow::Result<()> {
    let router = Router::new().route("/socket", get(connection::upgrade)).with_state(state);
    axum::serve(listener, router).await?;
    Ok(())
}
