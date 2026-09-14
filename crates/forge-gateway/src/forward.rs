//! The route handler: hello probe, path parsing, splice, re-header,
//! and the streaming forward.
//!
//! Routing rules, in the order they are checked:
//! - `<segments>/api/hello` answers 404 and never constructs the
//!   upstream leg - the CLI's connectivity probe must not become an
//!   authenticated request to a third party on a path that needs no
//!   auth.
//! - `<segments>/v1/messages` (query ignored) resolves the binding and
//!   forwards to the bound account's upstream with the real credential
//!   attached and the CLI's dummy stripped.
//! - An unregistered session is a loud 503 naming the session; the
//!   upstream leg is never constructed for it. Account selection
//!   arrives in 2c and turns this failure into the selection path.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use hyper::body::Frame;
use hyper::header::CONTENT_TYPE;
use hyper::{Method, Request, StatusCode};

use crate::account::AccountKey;
use crate::binding::{AUTH_TOKEN_VARIABLE, Bindings, DUMMY_CREDENTIAL, OAUTH_VARIABLE};
use crate::listener::{RouteHandler, StreamBody, text_response};
use crate::splice::splice_model;

/// The upstream an account with no base URL of its own talks to: the
/// four Anthropic accounts declare none today, and the CLI's default
/// host is what they have always used.
const ANTHROPIC_UPSTREAM: &str = "https://api.anthropic.com";

/// The error type a streamed body yields.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The gateway's route handler: bindings + the account pool + the
/// upstream client.
pub struct Gateway {
    pub bindings: Bindings,
    pool: Arc<crate::AccountPool>,
    client: reqwest::Client,
}

impl Gateway {
    pub fn new(pool: Arc<crate::AccountPool>) -> Self {
        Self { bindings: Bindings::default(), pool, client: reqwest::Client::new() }
    }

    /// Read the real credential and upstream base out of the bound
    /// account's env. `None` when the account carries no credential in
    /// the variable its provider authenticates with.
    fn credential_for(&self, account: &AccountKey) -> Option<(String, String)> {
        let state = self.pool.state();
        let account_state = state.by_key.get(account)?;
        let provider = account_state.provider;
        let variable = if provider.uses_base_url() { AUTH_TOKEN_VARIABLE } else { OAUTH_VARIABLE };
        let credential = account_state.env.get(variable)?.trim().to_owned();
        if credential.is_empty() || credential == DUMMY_CREDENTIAL {
            return None;
        }
        let upstream = account_state
            .env
            .get("ANTHROPIC_BASE_URL")
            .map(|v| v.trim().trim_end_matches('/').to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| ANTHROPIC_UPSTREAM.to_owned());
        Some((upstream, credential))
    }
}

#[async_trait::async_trait]
impl RouteHandler for Gateway {
    async fn route(&self, request: Request<Bytes>) -> hyper::Response<StreamBody> {
        let segments: Vec<&str> =
            request.uri().path().split('/').filter(|s| !s.is_empty()).collect();
        if segments.len() < 4 {
            return text_response(StatusCode::NOT_FOUND, "not found");
        }
        let (org, project, session) = (segments[0], segments[1], segments[2]);
        let api_path = segments[3..].join("/");

        // The CLI's connectivity probe. 404 is the answer it proceeds
        // on; no upstream leg, no credentials.
        if api_path == "api/hello" {
            return text_response(StatusCode::NOT_FOUND, "not found");
        }
        if request.method() != Method::POST || api_path != "v1/messages" {
            return text_response(StatusCode::NOT_FOUND, "not found");
        }

        let Some(account) = self.bindings.binding_for(org, project, session) else {
            return text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "session '{session}' is not registered with the gateway; its spawn has \
                     not bound an account yet"
                ),
            );
        };
        let Some((upstream, credential)) = self.credential_for(&account) else {
            return text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "account '{}' has no usable credential for session '{session}'; \
                     nothing was forwarded",
                    account.0
                ),
            );
        };

        let query = match request.uri().query() {
            Some(q) => format!("?{q}"),
            None => "?beta=true".to_owned(),
        };
        let body = request.into_body();
        let (spliced, _model) = match splice_model(&body, None) {
            Ok(spliced) => spliced,
            Err(error) => return text_response(StatusCode::BAD_REQUEST, error.to_string()),
        };

        let url = format!("{upstream}/v1/messages{query}");

        // reqwest derives Content-Length from the spliced bytes, so the
        // rewrite can never leave a stale length behind.
        let upstream_response = match self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(hyper::header::AUTHORIZATION, format!("Bearer {credential}"))
            .body(spliced)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return text_response(
                    StatusCode::BAD_GATEWAY,
                    format!("upstream request failed: {error}"),
                );
            }
        };

        let status = upstream_response.status();
        let content_type = upstream_response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut response = hyper::Response::builder()
            .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
        if let Some(content_type) = content_type {
            response = response.header(CONTENT_TYPE, content_type);
        }
        let stream: std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<Frame<Bytes>, BoxError>> + Send + Sync>,
        > = Box::pin(
            upstream_response
                .bytes_stream()
                .map(|chunk| chunk.map(Frame::data).map_err(|error| Box::new(error) as BoxError)),
        );
        let streamed: StreamBody =
            http_body_util::combinators::BoxBody::new(http_body_util::StreamBody::new(stream));
        match response.body(streamed) {
            Ok(response) => response,
            Err(error) => {
                text_response(StatusCode::BAD_GATEWAY, format!("response build failed: {error}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::Registration;
    use crate::listener::GatewayListener;
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::time::{Duration, Instant};

    /// One request the stub upstream received.
    #[derive(Debug, Clone)]
    struct Recorded {
        path_with_query: String,
        authorization: Option<String>,
        x_api_key: Option<String>,
        body: String,
    }

    /// A stub upstream that records what arrived and streams its
    /// response in two chunks separated by `chunk_delay`, so a
    /// buffering gateway is measurable from the client side.
    struct RecordingUpstream {
        requests: Arc<parking_lot::Mutex<Vec<Recorded>>>,
        chunk_delay: Duration,
    }

    #[async_trait::async_trait]
    impl RouteHandler for RecordingUpstream {
        async fn route(&self, request: Request<Bytes>) -> hyper::Response<StreamBody> {
            self.requests.lock().push(Recorded {
                path_with_query: request
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str().to_owned())
                    .unwrap_or_default(),
                authorization: request
                    .headers()
                    .get(hyper::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                x_api_key: request
                    .headers()
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                body: String::from_utf8_lossy(request.body()).into_owned(),
            });
            let delay = self.chunk_delay;
            let stream = futures_util::stream::unfold(
                (0u8, delay),
                move |state: (u8, Duration)| async move {
                    type BoxError = Box<dyn std::error::Error + Send + Sync>;
                    match state.0 {
                        0 => Some((
                            Ok::<Frame<Bytes>, BoxError>(Frame::data(Bytes::from_static(
                                b"event: first\n",
                            ))),
                            (1u8, delay),
                        )),
                        1 => {
                            tokio::time::sleep(delay).await;
                            Some((
                                Ok(Frame::data(Bytes::from_static(b"event: second\n"))),
                                (2u8, delay),
                            ))
                        }
                        _ => None,
                    }
                },
            );
            hyper::Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "text/event-stream")
                .body(http_body_util::combinators::BoxBody::new(http_body_util::StreamBody::new(
                    stream,
                )))
                .expect("static response parts always build")
        }
    }

    /// A free port, for the stub and the listener under test.
    async fn free_port() -> u16 {
        let probe = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("probe");
        probe.local_addr().expect("addr").port()
    }

    struct Harness {
        /// The URL a CLI was told to talk to.
        client_url: String,
        requests: Arc<parking_lot::Mutex<Vec<Recorded>>>,
    }

    /// Stub upstream + gateway listener + one registered session whose
    /// account's upstream is the stub.
    async fn harness(chunk_delay: Duration) -> Harness {
        let upstream = Arc::new(RecordingUpstream {
            requests: Arc::new(parking_lot::Mutex::new(Vec::new())),
            chunk_delay,
        });
        let requests = Arc::clone(&upstream.requests);
        let stub_port = free_port().await;
        let stub_listener = GatewayListener::bind(stub_port).await.expect("stub bind");
        let stub_url = format!("http://127.0.0.1:{stub_port}");
        let stub_handler: Arc<dyn RouteHandler> = upstream.clone();
        tokio::spawn(async move { stub_listener.run(stub_handler).await });

        let account_env = HashMap::from([
            ("ANTHROPIC_BASE_URL".to_owned(), stub_url),
            ("ANTHROPIC_AUTH_TOKEN".to_owned(), "real-openrouter-key".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), String::new()),
        ]);
        let pool = Arc::new(crate::AccountPool::new(&[forge_primitives::account::LoadedAccount {
            display_name: "OpenRouter".to_owned(),
            config_dir: std::path::PathBuf::from("/cfg/openrouter"),
            provider: forge_primitives::account::Provider::Openrouter,
            env: account_env.clone(),
            experimental: false,
        }]));
        let gateway = Arc::new(Gateway::new(Arc::clone(&pool)));

        let listener_port = free_port().await;
        let listener = GatewayListener::bind(listener_port).await.expect("gateway bind");
        let listener_base = format!("http://127.0.0.1:{listener_port}");
        let client_url = format!("{listener_base}/Busytools/forge/session-1");
        let gateway_handler: Arc<dyn RouteHandler> = gateway.clone();
        tokio::spawn(async move { listener.run(gateway_handler).await });

        gateway.bindings.register(
            &Registration {
                org: "Busytools".to_owned(),
                project: "forge".to_owned(),
                session: "session-1".to_owned(),
                account: AccountKey("OpenRouter".to_owned()),
                provider: forge_primitives::account::Provider::Openrouter,
            },
            &listener_base,
            &account_env,
        );

        Harness { client_url, requests }
    }

    /// A CLI-shaped POST: the dummy credential in the auth variable's
    /// header form and a junk `x-api-key`, exactly what a launching env
    /// with `ANTHROPIC_API_KEY` set would produce.
    async fn post_as_cli(url: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(url)
            .header(hyper::header::AUTHORIZATION, "Bearer forge-gateway-unused")
            .header("x-api-key", "junk-key")
            .header(CONTENT_TYPE, "application/json")
            .body(r#"{"model":"claude-opus-5","messages":[]}"#)
            .send()
            .await
            .expect("gateway responds")
    }

    #[tokio::test]
    async fn the_hello_probe_answers_404_and_never_reaches_the_upstream() {
        let harness = harness(Duration::ZERO).await;
        let response = reqwest::Client::new()
            .head(format!("{}/api/hello", harness.client_url))
            .send()
            .await
            .expect("gateway responds");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            harness.requests.lock().is_empty(),
            "the connectivity probe must not construct an upstream request",
        );
    }

    #[tokio::test]
    async fn a_forwarded_message_carries_the_real_credential_and_none_of_the_dummy() {
        let harness = harness(Duration::ZERO).await;
        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::OK);

        let requests = harness.requests.lock();
        assert_eq!(requests.len(), 1, "exactly one upstream request");
        let recorded = &requests[0];
        assert_eq!(
            recorded.path_with_query, "/v1/messages?beta=true",
            "the API path forwards with its query"
        );
        assert_eq!(
            recorded.authorization.as_deref(),
            Some("Bearer real-openrouter-key"),
            "the real credential rides the auth header",
        );
        assert_eq!(recorded.x_api_key, None, "x-api-key is stripped, never forwarded");
        assert!(!recorded.body.contains(DUMMY_CREDENTIAL), "the dummy never reaches the upstream");
    }

    #[tokio::test]
    async fn an_unregistered_session_is_a_loud_503_and_never_reaches_the_upstream() {
        let harness = harness(Duration::ZERO).await;
        let ghost_url = harness.client_url.replacen("session-1", "ghost", 1);
        let response = post_as_cli(&format!("{ghost_url}/v1/messages?beta=true")).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.expect("body");
        assert!(
            body.contains("ghost") && body.contains("not registered"),
            "the failure names the session and says it is not registered, got: {body}",
        );
        assert!(
            harness.requests.lock().is_empty(),
            "an unregistered session must not construct an upstream request",
        );
    }

    #[tokio::test]
    async fn the_response_streams_while_the_upstream_is_still_sending() {
        let harness = harness(Duration::from_millis(1000)).await;
        let started = Instant::now();
        let mut response =
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;

        let first = response.chunk().await.expect("a body chunk").expect("chunk bytes");
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "the first chunk arrives while the upstream is still streaming, not after it \
             finished; took {:?}",
            started.elapsed(),
        );
        assert!(!first.is_empty());

        // The tail arrives after the stub's gap, so the whole body
        // really was split across the stream.
        let second = response.chunk().await.expect("a second chunk").expect("chunk bytes");
        assert!(!second.is_empty());
        assert!(started.elapsed() >= Duration::from_millis(1000), "the tail respects the gap");
    }
}
