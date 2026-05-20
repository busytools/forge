//! Wire-classification rewriter proxy.
//!
//! Runs an in-process MITM HTTP proxy that intercepts the `claude`
//! subprocess's HTTPS traffic and normalises the 6 classification
//! signal channels documented in
//! `~/.claude/memory/reference_claude_cli_integration_modes.md`:
//!
//! 1. `GET /api/claude_cli/bootstrap?entrypoint=…` query string
//! 2. `User-Agent` on `POST /v1/messages`
//! 3. `User-Agent` on MCP `initialize` calls
//! 4. `POST /api/event_logging/v2/batch` body (`entrypoint`,
//!    `client_type`, `is_interactive`, `agent_sdk_version`)
//! 5. `POST /api/eval/sdk-…` Statsig body (`attributes.entrypoint`)
//! 6. `POST .../api/v2/logs` Datadog body + `ddtags`
//!
//! Empirically the CLI self-classifies via the `H9q` function (extracted
//! at offset ~184418075 in v2.1.133) based on `argv`/`isTTY`/env. We
//! cannot influence that decision without a TTY, but we CAN rewrite
//! the wire so Anthropic's tier classification matches what the
//! session actually is — a human at a terminal driving forge-tui.
//!
//! The forge approach is to embed the proxy inside `forge-sdk`. One
//! proxy per forge process; every spawned `claude` child inherits
//! `HTTPS_PROXY=http://127.0.0.1:<port>` and `NODE_EXTRA_CA_CERTS=…`
//! from the workspace-owned [`ProxyHandle`].

pub mod ca;
pub mod rewrite;
pub mod scan;

pub use ca::{ca_paths, ensure_ca, load_authority};
pub use rewrite::{
    rewrite_bootstrap_query, rewrite_datadog_logs, rewrite_event_logging,
    rewrite_statsig_features, rewrite_user_agent,
};
pub use scan::{Finding, FindingKind, scan, scan_and_warn};

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hudsucker::{
    Body, HttpContext, HttpHandler, RequestOrResponse,
    builder::ProxyBuilder,
    hyper::{Request, Response, Uri, header},
};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::Error;

/// Handle to a running rewriter proxy. Held by the workspace and
/// passed to every [`crate::Client::spawn`] so subprocesses inherit
/// the right env vars.
#[derive(Debug, Clone)]
pub struct ProxyHandle {
    listen_addr: SocketAddr,
    ca_cert_path: PathBuf,
    shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl ProxyHandle {
    /// HTTP URL the child should set as `HTTPS_PROXY`.
    #[must_use]
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.listen_addr)
    }

    /// Path to the CA cert the child needs in `NODE_EXTRA_CA_CERTS`
    /// so its rustls / Node TLS layer trusts our man-in-the-middle.
    #[must_use]
    pub fn ca_cert_path(&self) -> &std::path::Path {
        &self.ca_cert_path
    }

    /// Socket address the proxy is listening on.
    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Trigger graceful shutdown. Idempotent.
    pub fn shutdown(&self) {
        if let Some(tx) = self.shutdown.lock().take() {
            let _ = tx.send(());
        }
    }
}

/// Start a wire-classification rewriter proxy on a random localhost
/// port. Generates/loads the persistent CA, binds the listener,
/// returns once the proxy is ready to serve.
///
/// # Errors
///
/// Returns [`Error::Connection`] for any setup failure: CA dir not
/// writable, port-bind failure, TLS provider init failure, etc.
/// Forge's policy is hard-fail (no session starts without a healthy
/// proxy), so callers should propagate this error directly.
pub async fn start() -> Result<ProxyHandle, Error> {
    let (cert_path, key_path) = ensure_ca()?;
    let authority = load_authority(&cert_path, &key_path)?;

    // Bind to ephemeral port; OS picks a free one.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| {
        Error::Connection { reason: format!("rewriter proxy bind failed: {e}") }
    })?;
    let listen_addr = listener.local_addr().map_err(|e| Error::Connection {
        reason: format!("rewriter proxy local_addr failed: {e}"),
    })?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let proxy = ProxyBuilder::new()
        .with_listener(listener)
        .with_rustls_client()
        .with_ca(authority)
        .with_http_handler(Rewriter)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .build();

    tokio::spawn(async move {
        if let Err(e) = proxy.start().await {
            warn!("wire-rewriter proxy exited with error: {e}");
        }
    });

    info!(
        listen_addr = %listen_addr,
        ca = %cert_path.display(),
        "wire-classification rewriter proxy started"
    );

    Ok(ProxyHandle {
        listen_addr,
        ca_cert_path: cert_path,
        shutdown: Arc::new(Mutex::new(Some(shutdown_tx))),
    })
}

/// The HTTP handler. Statelessly inspects each request, rewrites if
/// the host + path match a known channel, runs the defensive scan,
/// forwards.
#[derive(Clone)]
struct Rewriter;

impl HttpHandler for Rewriter {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        match rewrite_request(req).await {
            Ok(req) => RequestOrResponse::Request(req),
            Err(e) => {
                warn!("wire-rewriter: request rewrite failed: {e}");
                // The original `req` was consumed by `rewrite_request`,
                // so we cannot forward the unmodified original. Return
                // a 502; the CLI's HTTP client surfaces this as a
                // transient API error and the user can retry.
                RequestOrResponse::Response(
                    Response::builder()
                        .status(502)
                        .body(error_body("forge wire-rewriter: internal error"))
                        .unwrap_or_else(|_| {
                            // Builder failure should be impossible
                            // here; fall back to an empty response
                            // rather than panicking.
                            Response::new(Body::empty())
                        }),
                )
            }
        }
    }

    async fn handle_response(
        &mut self,
        _ctx: &HttpContext,
        res: Response<Body>,
    ) -> Response<Body> {
        // CRITICAL: do NOT buffer. /v1/messages is text/event-stream
        // (SSE); buffering hangs the turn loop because the CLI is
        // streaming-aware. Classification is request-only anyway, so
        // there is nothing to rewrite on the way back.
        res
    }
}

fn body_from_bytes(b: Bytes) -> Body {
    Body::from(Full::new(b))
}

fn error_body(s: &'static str) -> Body {
    Body::from(s)
}

async fn rewrite_request(req: Request<Body>) -> Result<Request<Body>, String> {
    let host = req.uri().host().unwrap_or("").to_string();
    let path = req.uri().path().to_string();
    debug!(method = %req.method(), %host, %path, "wire-rewriter: request");

    let is_anthropic = host.ends_with("anthropic.com");
    let is_datadog = host.contains("datadoghq.com");
    let needs_inspection = is_anthropic || is_datadog;

    let (mut parts, body) = req.into_parts();

    // (1) Bootstrap query string rewrite.
    if is_anthropic
        && path.contains("/bootstrap")
        && let Some(q) = parts.uri.query()
        && let Some(new_q) = rewrite_bootstrap_query(q)
    {
        let scheme = parts.uri.scheme().cloned();
        let authority = parts.uri.authority().cloned();
        let new_path_q = format!("{path}?{new_q}");
        let mut builder = Uri::builder().path_and_query(new_path_q.as_str());
        if let Some(s) = scheme {
            builder = builder.scheme(s);
        }
        if let Some(a) = authority {
            builder = builder.authority(a);
        }
        match builder.build() {
            Ok(uri) => {
                debug!(old = %q, new = %uri.query().unwrap_or(""), "bootstrap qs rewritten");
                parts.uri = uri;
            }
            Err(e) => return Err(format!("uri rebuild failed: {e}")),
        }
    }

    // (2 + 3) User-Agent rewrite. Applies to /v1/messages and MCP init
    // alike — both touch anthropic.com endpoints and both carry the
    // classification label inside the parens.
    if is_anthropic
        && let Some(ua) = parts.headers.get(header::USER_AGENT).cloned()
        && let Ok(ua_str) = ua.to_str()
        && let Some(new_ua) = rewrite_user_agent(ua_str)
    {
        debug!(old = %ua_str, new = %new_ua, "user-agent rewritten");
        match new_ua.parse() {
            Ok(v) => {
                parts.headers.insert(header::USER_AGENT, v);
            }
            Err(e) => return Err(format!("ua parse failed: {e}")),
        }
    }

    // (4, 5, 6) Body rewrites.
    let body_bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return Err(format!("body collect failed: {e}")),
    };

    let new_body: Bytes = if body_bytes.is_empty() {
        body_bytes
    } else if is_anthropic && path.contains("/event_logging/") {
        rewrite_event_logging(&body_bytes)
    } else if is_anthropic && path.contains("/api/eval/") {
        rewrite_statsig_features(&body_bytes)
    } else if is_datadog && path.contains("/api/v2/logs") {
        rewrite_datadog_logs(&body_bytes)
    } else {
        body_bytes
    };

    // Defensive scan — flag anything sdk-* that slipped through. The
    // proxy logs at warn; the test harness asserts emptiness.
    if needs_inspection
        && !new_body.is_empty()
        && let Ok(v) = serde_json::from_slice::<Value>(&new_body)
    {
        scan_and_warn(&v, &path);
    }

    // Recompute Content-Length after any body mutation.
    let current_len = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    if current_len != Some(new_body.len()) {
        parts.headers.remove(header::CONTENT_LENGTH);
        match new_body.len().to_string().parse() {
            Ok(v) => {
                parts.headers.insert(header::CONTENT_LENGTH, v);
            }
            Err(e) => return Err(format!("content-length set failed: {e}")),
        }
    }

    Ok(Request::from_parts(parts, body_from_bytes(new_body)))
}
