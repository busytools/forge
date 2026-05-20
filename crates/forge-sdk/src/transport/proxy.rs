//! Wire-classification rewriter proxy.
//!
//! Runs an in-process MITM HTTP proxy that intercepts the `claude`
//! subprocess's HTTPS traffic and normalises the 6 classification
//! signal channels documented in
//! `~/.claude/memory/reference_claude_cli_integration_modes.md`:
//!
//! 1. `GET /api/claude_cli/bootstrap?entrypoint=...` query string
//! 2. `User-Agent` on `POST /v1/messages`
//! 3. `User-Agent` on MCP `initialize` calls
//! 4. `POST /api/event_logging/v2/batch` body (`entrypoint`,
//!    `client_type`, `is_interactive`, `agent_sdk_version`)
//! 5. `POST /api/eval/sdk-...` Statsig body (`attributes.entrypoint`)
//! 6. `POST .../api/v2/logs` Datadog body + `ddtags`
//!
//! Empirically the CLI self-classifies via the `H9q` function (extracted
//! at offset ~184418075 in v2.1.133) based on `argv`/`isTTY`/env. We
//! cannot influence that decision without a TTY, but we CAN rewrite
//! the wire so Anthropic's tier classification matches what the
//! session actually is - a human at a terminal driving forge-tui.
//!
//! The forge approach is to embed the proxy inside `forge-sdk`. One
//! proxy per forge process; every spawned `claude` child inherits
//! `HTTPS_PROXY=http://127.0.0.1:<port>` and `NODE_EXTRA_CA_CERTS=...`
//! from the workspace-owned [`ProxyHandle`].

pub mod ca;
pub mod rewrite;
pub mod scan;

pub use ca::{ca_paths, ensure_ca, load_authority};
pub use rewrite::{
    normalize_classification_fields, rewrite_anthropic_beta, rewrite_anthropic_unknown,
    rewrite_bootstrap_query, rewrite_datadog_logs, rewrite_event_logging, rewrite_messages_body,
    rewrite_statsig_features, rewrite_user_agent, strip_sdk_datadog_entries, strip_sdk_events,
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
use hyper_proxy2::{Intercept, Proxy as UpstreamProxy, ProxyConnector};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_rustls::TlsConnector;
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
/// **Upstream-proxy chaining.** If `HTTPS_PROXY` (or `https_proxy`)
/// is set in the parent environment at forge launch, the rewriter
/// routes its own outbound HTTPS through that upstream proxy. This
/// makes the mitmproxy-capture recipe symmetric: the same
/// `HTTPS_PROXY=http://127.0.0.1:8080` + `NODE_EXTRA_CA_CERTS=...`
/// env vars that capture traffic from a bare `claude` invocation
/// also capture forge's rewritten output. Without chaining, the
/// forge-spawned child speaks to forge's internal proxy and the
/// user's mitmproxy sees nothing.
///
/// # Errors
///
/// Returns [`Error::Connection`] for any setup failure: CA dir not
/// writable, port-bind failure, TLS provider init failure, or
/// invalid `HTTPS_PROXY` URL. Forge's policy is hard-fail (no
/// session starts without a healthy proxy), so callers should
/// propagate this error directly.
pub async fn start() -> Result<ProxyHandle, Error> {
    let (cert_path, key_path) = ensure_ca()?;
    let authority = load_authority(&cert_path, &key_path)?;

    // Bind to ephemeral port; OS picks a free one.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| Error::Connection { reason: format!("rewriter proxy bind failed: {e}") })?;
    let listen_addr = listener.local_addr().map_err(|e| Error::Connection {
        reason: format!("rewriter proxy local_addr failed: {e}"),
    })?;

    let upstream_proxy_url = detect_upstream_proxy_url();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    // Single source of truth for the outbound TLS trust store: webpki
    // roots plus any cert in NODE_EXTRA_CA_CERTS. Used by both the
    // chained (proxy CONNECT-tunnel TLS) and direct (HttpsConnector
    // TLS) paths so the rewriter works behind any MITM proxy
    // (mitmproxy, Zscaler, Palo Alto, corporate CA) - mirrors how
    // Node honours NODE_EXTRA_CA_CERTS for `claude`.
    let tls_config = Arc::new(build_outbound_tls_config()?);

    if let Some(upstream_url) = &upstream_proxy_url {
        info!(
            upstream = %upstream_url,
            "wire-rewriter: chaining outbound HTTPS through upstream proxy (HTTPS_PROXY)"
        );
        let client = build_chained_client(upstream_url, Arc::clone(&tls_config))?;
        let proxy = ProxyBuilder::new()
            .with_listener(listener)
            .with_client(client)
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
    } else {
        let https = HttpsConnectorBuilder::new()
            .with_tls_config((*tls_config).clone())
            .https_or_http()
            .enable_http1()
            .build();
        let client = HyperClient::builder(TokioExecutor::new())
            .http1_title_case_headers(true)
            .http1_preserve_header_case(true)
            .build(https);
        let proxy = ProxyBuilder::new()
            .with_listener(listener)
            .with_client(client)
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
    }

    info!(
        listen_addr = %listen_addr,
        ca = %cert_path.display(),
        upstream = upstream_proxy_url.as_deref().unwrap_or("(direct)"),
        "wire-classification rewriter proxy started"
    );

    Ok(ProxyHandle {
        listen_addr,
        ca_cert_path: cert_path,
        shutdown: Arc::new(Mutex::new(Some(shutdown_tx))),
    })
}

/// Read `HTTPS_PROXY` / `https_proxy` from env. Returns `None` when
/// neither is set or the value is empty. `HTTP_PROXY` is deliberately
/// not consulted, as the rewriter's outbound is HTTPS-only.
fn detect_upstream_proxy_url() -> Option<String> {
    for key in ["HTTPS_PROXY", "https_proxy"] {
        if let Ok(v) = std::env::var(key)
            && !v.trim().is_empty()
        {
            return Some(v);
        }
    }
    None
}

/// Build a hyper client whose connector tunnels through the given
/// upstream proxy URL.
///
/// **Critical**: hyper-proxy2's `rustls-webpki` feature builds an
/// internal `TlsConnector` with webpki-roots only - it ignores any
/// TLS config attached to the inner connector. We override that
/// internal connector via `set_tls()` so the in-tunnel TLS handshake
/// (i.e. the TLS to api.anthropic.com routed through mitmproxy)
/// uses OUR trust store (webpki + NODE_EXTRA_CA_CERTS). Without
/// this, mitmproxy's MITM cert fails validation and every forwarded
/// request 502s.
///
/// The inner connector is a plain `HttpConnector` because the proxy
/// itself is `http://127.0.0.1:9002` - no TLS to the proxy, only
/// in-tunnel TLS to the actual target.
fn build_chained_client(
    upstream_url: &str,
    tls_config: Arc<hudsucker::rustls::ClientConfig>,
) -> Result<HyperClient<ProxyConnector<HttpConnector>, Body>, Error> {
    let proxy_uri = upstream_url.parse::<Uri>().map_err(|e| Error::Connection {
        reason: format!("wire-rewriter: HTTPS_PROXY={upstream_url:?} is not a valid URI: {e}"),
    })?;
    let upstream = UpstreamProxy::new(Intercept::All, proxy_uri);

    let mut http = HttpConnector::new();
    http.enforce_http(false);
    let mut connector =
        ProxyConnector::from_proxy(http, upstream).map_err(|e| Error::Connection {
            reason: format!("wire-rewriter: building upstream-proxy connector failed: {e}"),
        })?;
    // Replace hyper-proxy2's default webpki-only TlsConnector with one
    // built from our extended trust store. This is the line that lets
    // the rewriter live behind a custom-CA MITM (mitmproxy, Zscaler,
    // Palo Alto, corporate root).
    connector.set_tls(Some(TlsConnector::from(tls_config)));

    Ok(HyperClient::builder(TokioExecutor::new())
        .http1_title_case_headers(true)
        .http1_preserve_header_case(true)
        .build(connector))
}

/// Construct a rustls `ClientConfig` for the rewriter's outbound TLS.
/// Starts with webpki-roots; if `NODE_EXTRA_CA_CERTS` is set in env,
/// every PEM cert in that file is added to the trust anchors. That
/// mirrors how the `claude` CLI (via Node's TLS layer) extends its
/// trust store, so the same env-var recipe works for both binaries.
///
/// `NODE_EXTRA_CA_CERTS` is a single-path env var (Node loads the
/// file as a concatenated PEM bundle). Loading failures are non-
/// fatal: we log a warn and continue with webpki-roots only.
fn build_outbound_tls_config() -> Result<hudsucker::rustls::ClientConfig, Error> {
    use hudsucker::rustls;
    use rustls_pki_types::TrustAnchor;

    let mut roots = rustls::RootCertStore::empty();
    for ta in webpki_roots::TLS_SERVER_ROOTS {
        roots.roots.push(TrustAnchor {
            subject: ta.subject.clone(),
            subject_public_key_info: ta.subject_public_key_info.clone(),
            name_constraints: ta.name_constraints.clone(),
        });
    }

    let extra_bundle_path = ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE"]
        .into_iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()).map(|v| (k, v)));

    if let Some((var, path)) = extra_bundle_path {
        match std::fs::read(&path) {
            Ok(pem) => {
                let mut slice = pem.as_slice();
                let mut added = 0usize;
                let mut parse_errors = 0usize;
                let mut add_errors = 0usize;
                for cert in rustls_pemfile::certs(&mut slice) {
                    match cert {
                        Ok(der) => match roots.add(der) {
                            Ok(()) => added += 1,
                            Err(_) => add_errors += 1,
                        },
                        Err(_) => parse_errors += 1,
                    }
                }
                if added == 0 {
                    warn!(
                        path = %path,
                        var,
                        parse_errors,
                        add_errors,
                        "wire-rewriter: extra-CA bundle read but no certs added to trust store; MITM proxy will fail TLS handshake to upstream"
                    );
                } else {
                    info!(
                        path = %path,
                        var,
                        added,
                        parse_errors,
                        add_errors,
                        "wire-rewriter: extended trust store with extra-CA bundle"
                    );
                }
            }
            Err(e) => warn!(
                path = %path,
                var,
                error = %e,
                "wire-rewriter: extra-CA bundle could not be read; continuing with webpki-roots only"
            ),
        }
    } else {
        debug!("wire-rewriter: no extra-CA env var set; trust store is webpki-roots only");
    }

    Ok(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth())
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
        // Local intercept: a handful of endpoints native `claude`
        // never hits but forge's spawned claude does (because the
        // CLI's internal classification is `sdk-cli`, gating
        // different feature surfaces). Returning a synthetic 200
        // keeps the request off the wire entirely - to any
        // third-party observer (mitmproxy, IDS, network log), forge
        // is indistinguishable from native at the endpoint-coverage
        // layer.
        if let Some(stub) = try_local_intercept(&req) {
            debug!(uri = %req.uri(), "wire-rewriter: local-intercept stub returned");
            return RequestOrResponse::Response(stub);
        }
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

    async fn handle_response(&mut self, _ctx: &HttpContext, res: Response<Body>) -> Response<Body> {
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

/// Returns a synthetic 200 response for endpoints native `claude`
/// doesn't hit but forge's spawned claude does (sdk-cli classification
/// gates these on differently). Returns None when the request should
/// flow through normally.
///
/// Currently intercepted (Category B1, B3 from the wire-equivalence audit):
/// - `POST /v1/messages/count_tokens` - claude pre-flights token
///   estimates per turn in sdk-cli mode; native skips it entirely.
///   Stub returns `{"input_tokens": 0}` which claude treats as a
///   non-informative estimate and proceeds normally.
/// - `GET /api/claude_code/organizations/metrics_enabled` - claude
///   probes org metrics state in sdk-cli mode; native probes a
///   different endpoint (`claude_code_penguin_mode`). Stub returns
///   `{"enabled": false}`.
fn try_local_intercept(req: &Request<Body>) -> Option<Response<Body>> {
    let host = req.uri().host().unwrap_or("");
    if !host.ends_with("anthropic.com") {
        return None;
    }
    let path = req.uri().path();
    if path.contains("/v1/messages/count_tokens") {
        return Some(stub_json_response(br#"{"input_tokens":0}"#));
    }
    if path.contains("/claude_code/organizations/metrics_enabled") {
        return Some(stub_json_response(br#"{"enabled":false}"#));
    }
    None
}

fn stub_json_response(body: &'static [u8]) -> Response<Body> {
    let bytes = Bytes::from_static(body);
    let len = bytes.len();
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, len)
        .body(body_from_bytes(bytes))
        .unwrap_or_else(|_| Response::new(Body::empty()))
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

    // Universal User-Agent rewrite. Applies to EVERY outbound request
    // regardless of host. The CLI's `(external, sdk-cli, agent-sdk/X)`
    // UA on /v1/messages is one source; forge-sdk's MCP client's own
    // `(sdk-cli, agent-sdk/X)` UA on third-party MCP hosts
    // (mcp.context7.com, api.greptile.com, custom user MCPs) is the
    // other. Both must be normalised for any third-party observer to
    // be unable to distinguish forge from native claude.
    if let Some(ua) = parts.headers.get(header::USER_AGENT).cloned()
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

    // anthropic-beta header: strip forge-only beta flags that native
    // interactive `claude` never requests. Anthropic's API server
    // sees explicitly different feature requests when forge asks for
    // `effort-2025-11-24`, `afk-mode-2026-01-31`, etc. - those flags
    // uniquely identify the session as forge-driven. Restricted to
    // Anthropic hosts because the header is Anthropic-specific.
    if is_anthropic
        && let Some(beta) = parts.headers.get("anthropic-beta").cloned()
        && let Ok(beta_str) = beta.to_str()
        && let Some(new_beta) = rewrite_anthropic_beta(beta_str)
    {
        debug!(old = %beta_str, new = %new_beta, "anthropic-beta rewritten");
        match new_beta.parse() {
            Ok(v) => {
                parts.headers.insert("anthropic-beta", v);
            }
            Err(e) => return Err(format!("anthropic-beta parse failed: {e}")),
        }
    }

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

    // Body rewrites. Per-path dispatch.
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
    } else if is_anthropic && path.contains("/v1/messages") {
        // The CLI bakes its self-classified entrypoint into the
        // /v1/messages system prompt as a substring like
        // `cc_entrypoint=sdk-cli` - once per turn, never via the JSON
        // key-rewrite path. Handle that AND apply the recursive
        // classification walker for any structured fields.
        rewrite_messages_body(&body_bytes)
    } else if is_anthropic {
        // Catch-all for unknown Anthropic endpoints. Applies the
        // recursive normaliser so a new classification surface
        // Anthropic adds gets normalised without a code change here.
        rewrite::rewrite_anthropic_unknown(&body_bytes)
    } else {
        body_bytes
    };

    // Defensive scan - flag anything sdk-* that slipped through. The
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
