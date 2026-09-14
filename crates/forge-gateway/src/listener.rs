//! The gateway's inference listener: loopback only, one fixed port.
//!
//! The spawned `claude` CLI is pointed here by `ANTHROPIC_BASE_URL`.
//! The listener never picks a port of its own: a child pointed at an
//! address the gateway did not bind fails later and illegibly, which
//! is exactly what preflight exists to prevent.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::Request;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;

/// A streamed response body: bytes off the upstream, errors boxed.
pub type StreamBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Routes one request. The request body arrives fully read - the CLI's
/// request is a single JSON document the splice needs all of - and the
/// response body streams.
#[async_trait::async_trait]
pub trait RouteHandler: Send + Sync {
    async fn route(&self, request: Request<Bytes>) -> hyper::Response<StreamBody>;
}

/// Errors the listener raises before any routing happens.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("gateway port {port} is taken; forge cannot redirect session traffic without it")]
    PortTaken {
        port: u16,
        #[source]
        source: std::io::Error,
    },
}

/// The bound loopback listener.
#[derive(Debug)]
pub struct GatewayListener {
    listener: tokio::net::TcpListener,
    addr: SocketAddr,
}

impl GatewayListener {
    /// Bind `127.0.0.1:port`. A taken port is an error, never a drift
    /// to an OS-assigned port.
    pub async fn bind(port: u16) -> Result<GatewayListener, GatewayError> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|source| GatewayError::PortTaken { port, source })?;
        Ok(Self { listener, addr })
    }

    /// The base URL children are stamped with.
    pub fn local_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Drive the accept loop. Runs until the process ends; every
    /// connection is served on its own task with keep-alive.
    pub async fn run(self, handler: Arc<dyn RouteHandler>) {
        loop {
            let Ok((stream, _)) = self.listener.accept().await else { continue };
            let io = TokioIo::new(stream);
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let handler = Arc::clone(&handler);
                    async move { route_via(&handler, request).await }
                });
                let _ =
                    hyper::server::conn::http1::Builder::new().serve_connection(io, service).await;
            });
        }
    }
}

/// Read the request body to bytes and hand the whole request to the
/// handler. Response bodies stream, so nothing downstream buffers.
async fn route_via(
    handler: &Arc<dyn RouteHandler>,
    request: Request<Incoming>,
) -> Result<hyper::Response<StreamBody>, std::convert::Infallible> {
    let (parts, body) = request.into_parts();
    let bytes = match http_body_util::BodyExt::collect(body).await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return Ok(text_response(
                hyper::StatusCode::BAD_REQUEST,
                format!("request body read failed: {error}"),
            ));
        }
    };
    Ok(handler.route(Request::from_parts(parts, bytes)).await)
}

/// A plain-text error response.
pub fn text_response(
    status: hyper::StatusCode,
    body: impl Into<Bytes>,
) -> hyper::Response<StreamBody> {
    hyper::Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(BoxBody::new(http_body_util::Full::new(body.into()).map_err(|never| match never {})))
        .expect("static response parts always build")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Find a genuinely free port by asking the OS for one and letting
    /// it go. Test machinery only - the production bind has no port-0
    /// path, which is the rule the second half of this test pins.
    async fn free_port() -> u16 {
        let probe = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("probe");
        probe.local_addr().expect("addr").port()
    }

    #[tokio::test]
    async fn a_taken_port_is_an_error_not_a_fallback() {
        let port = free_port().await;
        let first = GatewayListener::bind(port).await.expect("first bind owns the port");
        assert_eq!(
            first.local_url(),
            format!("http://127.0.0.1:{port}"),
            "the stamped base URL is the port that was asked for",
        );
        match GatewayListener::bind(port).await {
            Err(GatewayError::PortTaken { port: reported, .. }) => {
                assert_eq!(reported, port, "the error names the port it could not take");
            }
            other => panic!("the second bind on a taken port must fail, got: {other:?}"),
        }
    }

    /// A stub handler that records nothing and answers 200, so the
    /// hyper wiring itself is exercised end to end.
    struct AcceptAll;

    #[async_trait::async_trait]
    impl RouteHandler for AcceptAll {
        async fn route(&self, _request: Request<Bytes>) -> hyper::Response<StreamBody> {
            text_response(hyper::StatusCode::OK, "ok")
        }
    }

    #[tokio::test]
    async fn the_listener_serves_a_request_through_the_handler() {
        let port = free_port().await;
        let listener = GatewayListener::bind(port).await.expect("bind");
        let addr = listener.local_addr();
        tokio::spawn(async move { listener.run(Arc::new(AcceptAll)).await });

        let response = reqwest::get(format!("http://{addr}/anything")).await.expect("response");
        assert_eq!(response.status(), hyper::StatusCode::OK);
        assert_eq!(response.text().await.expect("body"), "ok");
    }
}
