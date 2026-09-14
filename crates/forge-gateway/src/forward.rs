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
//!   attached and the CLI's dummy stripped. An unbound session selects
//!   an account now, from the model in its own body.
//! - The failing response streams back to the CLI untouched, but the
//!   triggers it carries mark the account exhausted and rotate the
//!   binding, so the CLI's retry lands on the next account with
//!   nothing replayed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures_util::StreamExt;
use hyper::body::Frame;
use hyper::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use hyper::{HeaderMap, Method, Request, StatusCode};

use crate::account::AccountKey;
use crate::binding::{AUTH_TOKEN_VARIABLE, Bindings, DUMMY_CREDENTIAL, OAUTH_VARIABLE};
use crate::listener::{RouteHandler, StreamBody, text_response};
use crate::rotation::RotationState;
use crate::splice::splice_model;

/// The upstream an account with no base URL of its own talks to: the
/// four Anthropic accounts declare none today, and the CLI's default
/// host is what they have always used.
const ANTHROPIC_UPSTREAM: &str = "https://api.anthropic.com";

/// The error type a streamed body yields.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Why a request could not be bound to an account.
enum SelectFailure {
    /// The org in the path is not in the config.
    UnknownOrg { org: String },
    /// Every account in the walk fails the family rule for the model.
    NoEligibleAccount { model: String, org: String },
    /// The whole walk is inside a rotation cooldown: they serve the
    /// model, they are cooling until `reset_in`.
    BudgetExhausted { tried: usize, org: String, model: String, reset_in: Duration },
}

/// The gateway's route handler: bindings + the account pool +
/// rotation state + the upstream client.
pub struct Gateway {
    pub bindings: Bindings,
    pool: Arc<crate::AccountPool>,
    client: reqwest::Client,
    /// Each org's walk order, from the config the caller loaded. Read
    /// when a session has no binding and selection must run.
    org_pins: parking_lot::Mutex<HashMap<String, crate::selection::OrgPin>>,
    /// Exhaustion marks and cooldowns, shared with the workspace's
    /// `rate_limit_event` reports. The numbers inside arrive from
    /// `[gateway]` at boot and never change mid-run.
    rotation: parking_lot::Mutex<RotationState>,
}

impl Gateway {
    pub fn new(pool: Arc<crate::AccountPool>) -> Self {
        Self {
            bindings: Bindings::default(),
            pool,
            client: reqwest::Client::new(),
            org_pins: parking_lot::Mutex::new(HashMap::new()),
            rotation: parking_lot::Mutex::new(RotationState::new()),
        }
    }

    /// The rotation numbers from `[gateway]`: the 429 streak count and
    /// window, and the cooldown when no reset time is known. Called
    /// once at boot beside the org pins.
    pub fn set_rotation_numbers(&self, numbers: crate::rotation::RotationNumbers) {
        *self.rotation.lock() = RotationState::with_numbers(numbers);
    }

    /// Publish each org's walk order (primary pin then fallbacks).
    /// Called once at construction from the loaded config; the config
    /// is boot-frozen, so the table never changes mid-run.
    pub fn set_org_pins(&self, pins: impl IntoIterator<Item = (String, crate::selection::OrgPin)>) {
        *self.org_pins.lock() = pins.into_iter().collect();
    }

    /// Select the account for an unbound session: the model's family
    /// decides the eligible set, the org's walk order decides which of
    /// those wins, and accounts inside a rotation cooldown are skipped.
    /// Binds the result so the session keeps it.
    fn select_and_bind(
        &self,
        org: &str,
        project: &str,
        session: &str,
        model: &str,
    ) -> Result<AccountKey, SelectFailure> {
        let Some(pin) = self.org_pins.lock().get(org).cloned() else {
            return Err(SelectFailure::UnknownOrg { org: org.to_owned() });
        };
        let now = SystemTime::now();
        let rotation = self.rotation.lock();
        let cooling = |name: &String| rotation.is_cooling_down(&AccountKey(name.clone()), now);
        let cooled: Vec<String> = pin
            .accounts
            .iter()
            .chain(pin.fallback_accounts.iter())
            .filter(|n| cooling(n))
            .cloned()
            .collect();
        let pin = crate::selection::OrgPin {
            accounts: pin.accounts.into_iter().filter(|name| !cooling(name)).collect(),
            fallback_accounts: pin
                .fallback_accounts
                .into_iter()
                .filter(|name| !cooling(name))
                .collect(),
        };
        let account = match self.pool.select_account(&pin, org, model) {
            Ok(account) => account,
            // The walk emptied. When cooling accounts were filtered out,
            // the real cause is exhaustion: they DO serve the model,
            // they are cooling until a knowable time, and the budget
            // failure says so. Only a walk empty without cooling is the
            // family rule refusing the model.
            Err(crate::selection::SelectionError::NoEligibleAccount { model, org }) => {
                if let Some(reset) = rotation.soonest_reset(now).filter(|_| !cooled.is_empty()) {
                    return Err(SelectFailure::BudgetExhausted {
                        tried: cooled.len(),
                        org,
                        model,
                        reset_in: reset.duration_since(now).unwrap_or(Duration::ZERO),
                    });
                }
                return Err(SelectFailure::NoEligibleAccount { model, org });
            }
        };
        self.bindings.bind(org, project, session, account.clone());
        Ok(account)
    }

    /// Read the real credential and upstream base out of the bound
    /// account's env. `None` when the account carries no credential in
    /// the variable its provider authenticates with.
    fn credential_for(&self, account: &AccountKey) -> Option<(String, String)> {
        let (provider, env) = self.pool.provider_and_env(account)?;
        let variable = if provider.uses_base_url() { AUTH_TOKEN_VARIABLE } else { OAUTH_VARIABLE };
        let credential = env.get(variable)?.trim().to_owned();
        if credential.is_empty() || credential == DUMMY_CREDENTIAL {
            return None;
        }
        let upstream = env
            .get("ANTHROPIC_BASE_URL")
            .map(|v| v.trim().trim_end_matches('/').to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| ANTHROPIC_UPSTREAM.to_owned());
        Some((upstream, credential))
    }

    /// The reset the account's own usage probe reports for its
    /// exhausted window, when still ahead. Absent for every
    /// non-window-billed account.
    fn probe_reset_for(&self, account: &AccountKey) -> Option<Duration> {
        let now = SystemTime::now();
        let reset = self.pool.usage(&account.0)?.binding_reset_at()?;
        reset.duration_since(now).ok()
    }

    /// The cooldown for a failing response: the response's Retry-After,
    /// else the account's own probe reset, else the configured default.
    fn cooldown_for(&self, account: &AccountKey, headers: &HeaderMap) -> Duration {
        retry_after(headers)
            .or_else(|| self.probe_reset_for(account))
            .unwrap_or_else(|| self.rotation.lock().no_reset_cooldown())
    }

    /// Whether the bound account's family serves `model`. An unknown
    /// account serves nothing: the binding is stale and re-selects.
    fn binding_serves(&self, account: &AccountKey, model: &str) -> bool {
        self.pool
            .provider_and_env(account)
            .is_some_and(|(provider, _)| crate::selection::family_matches(provider, model))
    }

    /// Selection, or the loud response its failure produces.
    fn select_or_fail(
        &self,
        org: &str,
        project: &str,
        session: &str,
        model: &str,
    ) -> Result<AccountKey, Box<hyper::Response<StreamBody>>> {
        match self.select_and_bind(org, project, session, model) {
            Ok(account) => Ok(account),
            Err(SelectFailure::UnknownOrg { org }) => Err(Box::new(text_response(
                StatusCode::NOT_FOUND,
                format!("org '{org}' is not known to the gateway"),
            ))),
            Err(SelectFailure::NoEligibleAccount { model, org }) => Err(Box::new(text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "no account in org '{org}' serves model '{model}'; the session is \
                     unbound and nothing was forwarded"
                ),
            ))),
            Err(SelectFailure::BudgetExhausted { tried, org, model, reset_in }) => {
                Err(Box::new(text_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "every account serving model '{model}' in org '{org}' is cooling; \
                         tried {tried} account(s), soonest reset in {}s",
                        reset_in.as_secs()
                    ),
                )))
            }
        }
    }

    /// Apply the rotation triggers the held response proves, then
    /// stream it back untouched - the CLI's retry carries the same
    /// session id, so rotating the binding here is what routes the
    /// retry to the next account.
    fn note_response(
        &self,
        account: &AccountKey,
        org: &str,
        project: &str,
        session: &str,
        status: StatusCode,
        headers: &HeaderMap,
    ) {
        if status == StatusCode::TOO_MANY_REQUESTS {
            let now = SystemTime::now();
            if self.rotation.lock().record_429(account, now) {
                let cooldown = self.cooldown_for(account, headers);
                self.rotate_off(account, org, project, session, now + cooldown);
            }
        } else {
            if !status.is_success()
                && (header_value_is(headers, "anthropic-ratelimit-unified-status", "rejected")
                    || header_value_is(
                        headers,
                        "anthropic-ratelimit-unified-overage-status",
                        "rejected",
                    ))
            {
                let now = SystemTime::now();
                let cooldown = self.cooldown_for(account, headers);
                self.rotate_off(account, org, project, session, now + cooldown);
            }
            if status.is_success() {
                self.rotation.lock().reset_streak(account);
            }
        }
    }

    /// Mark `account` exhausted until `until` and drop the session's
    /// binding, so the next request for it re-selects with the
    /// exhausted account skipped.
    fn rotate_off(
        &self,
        account: &AccountKey,
        org: &str,
        project: &str,
        session: &str,
        until: SystemTime,
    ) {
        self.rotation.lock().cool_down(account, until);
        self.bindings.unbind(org, project, session);
    }

    /// The workspace's `rate_limit_event` report: the CLI told us the
    /// bound account's window is not `allowed`, and the frame's reset
    /// time is the cooldown. Both the exhausted account and the probe
    /// agree on a reset time or its absence; nothing else is read.
    /// A session with no binding (gateway not in use) has nothing to
    /// rotate.
    pub fn report_rate_limit(&self, session: &str, reset_at: Option<u64>) {
        let Some(account) = self.bindings.unbind_for_session(session) else {
            return;
        };
        let now = SystemTime::now();
        // A zero reset means no usable reset: it would end the cooldown
        // the instant it starts, so the configured default applies.
        let until = reset_at
            .filter(|secs| *secs > 0)
            .map_or(now + self.rotation.lock().no_reset_cooldown(), crate::rotation::reset_instant);
        self.rotation.lock().cool_down(&account, until);
    }

    /// The usage probe's verdict: a snapshot with any window at the
    /// cap and a reset ahead proves exhaustion until that reset. Every
    /// session bound to the account rotates.
    pub fn report_probe_limit(&self, account: &AccountKey, reset_at: Option<u64>) {
        let now = SystemTime::now();
        let until = reset_at
            .filter(|secs| *secs > 0)
            .map_or(now + self.rotation.lock().no_reset_cooldown(), crate::rotation::reset_instant);
        self.rotation.lock().cool_down(account, until);
        self.bindings.unbind_for_account(account);
    }
}

/// The `Retry-After` header, as seconds. Zero reads as absent: a
/// cooldown ending the instant it starts is the same as none, and the
/// next arm of the cooldown rule should decide instead.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(hyper::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
}

/// Case-insensitive header comparison against a wire literal.
fn header_value_is(headers: &HeaderMap, name: &str, value: &str) -> bool {
    headers.get(name).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case(value))
}

#[async_trait::async_trait]
impl RouteHandler for Gateway {
    async fn route(&self, request: Request<Bytes>) -> hyper::Response<StreamBody> {
        let (parts, body) = request.into_parts();
        let segments: Vec<&str> = parts.uri.path().split('/').filter(|s| !s.is_empty()).collect();
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
        if parts.method != Method::POST || api_path != "v1/messages" {
            return text_response(StatusCode::NOT_FOUND, "not found");
        }

        let (spliced, model) = match splice_model(&body, None) {
            Ok(spliced) => spliced,
            Err(error) => return text_response(StatusCode::BAD_REQUEST, error.to_string()),
        };

        // A binding keeps the session only while its account can serve
        // the model in the body: a model change the bound account
        // cannot serve rotates here, with the prompt-cache miss
        // accepted. An unbound session selects now, from the model in
        // its own body and the org named in its path. The selection
        // failure names both.
        let account = match self.bindings.binding_for(org, project, session) {
            Some(bound) if self.binding_serves(&bound, &model) => bound,
            Some(bound) => {
                tracing::info!(
                    target: "forge_gateway::forward",
                    account = %bound.0,
                    "the bound account cannot serve the model in the body; re-selecting"
                );
                self.bindings.unbind(org, project, session);
                match self.select_or_fail(org, project, session, &model) {
                    Ok(account) => account,
                    Err(response) => return *response,
                }
            }
            None => match self.select_or_fail(org, project, session, &model) {
                Ok(account) => account,
                Err(response) => return *response,
            },
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

        let query = match parts.uri.query() {
            Some(q) => format!("?{q}"),
            None => "?beta=true".to_owned(),
        };

        let url = format!("{upstream}/v1/messages{query}");

        // The CLI's headers forward verbatim - anthropic-beta,
        // anthropic-version, user-agent, x-app: the betas and the API
        // version are entitlements the upstream validates, and dropping
        // them changes what the request is allowed to do. The
        // credentials are the exception: both header forms are stripped
        // (HeaderMap keys are case-insensitive, and remove() clears
        // every value under the name), then the real credential is
        // attached. Framing headers go: reqwest derives them from the
        // spliced bytes, which is what keeps a rewrite from leaving a
        // stale length behind.
        let mut outbound_headers = parts.headers;
        while outbound_headers.remove(hyper::header::AUTHORIZATION).is_some() {}
        while outbound_headers.remove("x-api-key").is_some() {}
        outbound_headers.remove(hyper::header::HOST);
        outbound_headers.remove(CONTENT_LENGTH);
        outbound_headers.remove(hyper::header::TRANSFER_ENCODING);

        let upstream_response = match self
            .client
            .post(url)
            .headers(outbound_headers)
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

        // The upstream's headers stream back with the status -
        // retry-after, request-id and the rate-limit set are the CLI's
        // backoff inputs, and dropping them degrades it to blind
        // pacing. Framing headers are omitted: hyper re-frames the
        // streamed body itself.
        let status = upstream_response.status();
        let response_headers = upstream_response.headers().clone();
        self.note_response(&account, org, project, session, status, &response_headers);
        let mut response = hyper::Response::builder()
            .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
        for (name, value) in upstream_response.headers() {
            if name == CONTENT_LENGTH || name == TRANSFER_ENCODING {
                continue;
            }
            response = response.header(name, value);
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
    use hyper::header::CONTENT_TYPE;
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::time::{Duration, Instant};

    /// One request the stub upstream received.
    #[derive(Debug, Clone)]
    struct Recorded {
        path_with_query: String,
        headers: HashMap<String, Vec<String>>,
        body: String,
    }

    impl Recorded {
        fn all(&self, name: &str) -> Vec<&str> {
            self.headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .flat_map(|(_, v)| v.iter().map(String::as_str))
                .collect()
        }

        fn first(&self, name: &str) -> Option<&str> {
            self.all(name).into_iter().next()
        }
    }

    /// A stub upstream that records what arrived and streams its
    /// response in two chunks separated by `chunk_delay`, so a
    /// buffering gateway is measurable from the client side.
    struct RecordingUpstream {
        requests: Arc<parking_lot::Mutex<Vec<Recorded>>>,
        chunk_delay: Duration,
        /// One scripted (status, extra headers) pair per request.
        script: ScriptHandle,
    }

    type ScriptHandle =
        Arc<parking_lot::Mutex<std::collections::VecDeque<(u16, Vec<(String, String)>)>>>;

    #[async_trait::async_trait]
    impl RouteHandler for RecordingUpstream {
        async fn route(&self, request: Request<Bytes>) -> hyper::Response<StreamBody> {
            let mut headers: HashMap<String, Vec<String>> = HashMap::new();
            for (name, value) in request.headers() {
                let entry = headers.entry(name.as_str().to_ascii_lowercase()).or_default();
                entry.push(String::from_utf8_lossy(value.as_bytes()).into_owned());
            }
            self.requests.lock().push(Recorded {
                path_with_query: request
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str().to_owned())
                    .unwrap_or_default(),
                headers,
                body: String::from_utf8_lossy(request.body()).into_owned(),
            });
            // Each scripted (status, headers) pair answers one request;
            // unscripted requests get the healthy default. A scripted
            // header overrides its default rather than stacking a
            // second value under the same name.
            let (status, extra) =
                self.script.lock().pop_front().unwrap_or((StatusCode::OK.as_u16(), Vec::new()));
            let scripted = |name: &str| extra.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
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
            let mut builder = hyper::Response::builder()
                .status(StatusCode::from_u16(status).expect("test status"))
                .header(CONTENT_TYPE, "text/event-stream")
                .header("request-id", "req-test");
            if !scripted("retry-after") {
                builder = builder.header("retry-after", "30");
            }
            if !scripted("anthropic-ratelimit-unified-status") {
                builder = builder.header("anthropic-ratelimit-unified-status", "allowed");
            }
            for (name, value) in extra {
                builder = builder.header(name, value);
            }
            builder
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
        gateway: Arc<Gateway>,
        requests: Arc<parking_lot::Mutex<Vec<Recorded>>>,
        script: ScriptHandle,
    }

    /// Stub upstream + gateway listener + one registered session whose
    /// account's upstream is the stub. The pool holds one account per
    /// family, both with the stub as their upstream, so a test that
    /// asserts "no upstream request" is airtight whichever account a
    /// broken selection could have picked.
    async fn harness(chunk_delay: Duration) -> Harness {
        let script: ScriptHandle =
            Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let upstream = Arc::new(RecordingUpstream {
            requests: Arc::clone(&requests),
            chunk_delay,
            script: Arc::clone(&script),
        });
        let stub_port = free_port().await;
        let stub_listener = GatewayListener::bind(stub_port).await.expect("stub bind");
        let stub_url = format!("http://127.0.0.1:{stub_port}");
        let stub_handler: Arc<dyn RouteHandler> = upstream.clone();
        tokio::spawn(async move { stub_listener.run(stub_handler).await });

        let account_env = HashMap::from([
            ("ANTHROPIC_BASE_URL".to_owned(), stub_url.clone()),
            ("ANTHROPIC_AUTH_TOKEN".to_owned(), "real-openrouter-key".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), String::new()),
        ]);
        let anthropic_env = HashMap::from([
            ("ANTHROPIC_BASE_URL".to_owned(), stub_url),
            ("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "real-oauth-token".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), String::new()),
        ]);
        let pool = Arc::new(crate::AccountPool::new(&[
            forge_primitives::account::LoadedAccount {
                display_name: "OpenRouter".to_owned(),
                config_dir: std::path::PathBuf::from("/cfg/openrouter"),
                provider: forge_primitives::account::Provider::Openrouter,
                env: account_env.clone(),
                experimental: false,
            },
            forge_primitives::account::LoadedAccount {
                display_name: "Anthropic".to_owned(),
                config_dir: std::path::PathBuf::from("/cfg/anthropic"),
                provider: forge_primitives::account::Provider::Anthropic,
                env: anthropic_env,
                experimental: false,
            },
        ]));
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

        Harness { client_url, gateway, requests, script }
    }

    /// A CLI-shaped POST: the dummy credential in the auth variable's
    /// header form, a junk `x-api-key`, and the entitlement headers the
    /// real CLI emits - exactly what a launching env with
    /// `ANTHROPIC_API_KEY` set would produce.
    async fn post_as_cli(url: &str) -> reqwest::Response {
        // The harness session is bound to the OpenRouter account, so
        // the body carries that account's family: a claude model here
        // would rotate the binding instead of forwarding.
        reqwest::Client::new()
            .post(url)
            .header(hyper::header::AUTHORIZATION, "Bearer forge-gateway-unused")
            .header("x-api-key", "junk-key")
            .header(CONTENT_TYPE, "application/json")
            .header("anthropic-beta", "oauth-2025-04-20,extended-cache-ttl-2025-04-11")
            .header("anthropic-version", "2023-06-01")
            .header("user-agent", "claude-code/2.1.263")
            .body(r#"{"model":"glm-5.3-flash","messages":[]}"#)
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
    async fn a_forwarded_message_carries_the_cli_headers_and_the_real_credential_only() {
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
            recorded.first("authorization"),
            Some("Bearer real-openrouter-key"),
            "the real credential rides the auth header",
        );
        assert_eq!(
            recorded.all("x-api-key"),
            Vec::<&str>::new(),
            "x-api-key is stripped, never forwarded",
        );
        assert!(!recorded.body.contains(DUMMY_CREDENTIAL), "the dummy never reaches the upstream");
        // The entitlement headers the spec's wire section rests on:
        // they forward verbatim or an OAuth forward loses its beta and
        // a versioned API loses its version.
        assert_eq!(
            recorded.first("anthropic-beta"),
            Some("oauth-2025-04-20,extended-cache-ttl-2025-04-11"),
            "anthropic-beta forwards verbatim",
        );
        assert_eq!(
            recorded.first("anthropic-version"),
            Some("2023-06-01"),
            "anthropic-version forwards verbatim",
        );
        assert_eq!(
            recorded.first("user-agent"),
            Some("claude-code/2.1.263"),
            "user-agent forwards verbatim",
        );
        assert_eq!(
            recorded.first("content-type"),
            Some("application/json"),
            "content-type forwards verbatim",
        );
    }

    #[tokio::test]
    async fn upstream_response_headers_reach_the_cli() {
        let harness = harness(Duration::ZERO).await;
        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        // retry-after is the CLI's backoff input; the rate-limit set is
        // its window display. Dropping them degrades the client to
        // blind pacing.
        for (name, expected) in
            [("retry-after", "30"), ("anthropic-ratelimit-unified-status", "allowed")]
        {
            assert_eq!(
                response.headers().get(name).and_then(|v| v.to_str().ok()),
                Some(expected),
                "{name} must reach the CLI",
            );
        }
    }

    #[tokio::test]
    async fn an_unbound_session_selects_from_the_org_pin_and_binds() {
        let harness = harness(Duration::ZERO).await;
        // The harness registered session-1 → Openrouter. A ghost
        // session with no binding selects now: the model is
        // non-claude, so the first non-Anthropic account in walk order
        // wins, and the result is bound for the session's lifetime.
        harness.gateway.set_org_pins([(
            "Busytools".to_owned(),
            crate::selection::OrgPin {
                accounts: vec!["OpenRouter".to_owned(), "Stargate".to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
        // The harness pool's OpenRouter account defaults to Loading;
        // selection skips non-terminal accounts.
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("OpenRouter".to_owned()), crate::LoadingState::Ready);

        let ghost_url = harness.client_url.replacen("session-1", "ghost", 1);
        let response = reqwest::Client::new()
            .post(format!("{ghost_url}/v1/messages?beta=true"))
            .header(CONTENT_TYPE, "application/json")
            .body(r#"{"model":"glm-5.3-flash","messages":[]}"#)
            .send()
            .await
            .expect("gateway responds");
        let status = response.status();
        let failure_body = response.text().await.expect("body");
        assert_eq!(status, StatusCode::OK, "selection serves the request: {failure_body}");
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "ghost"),
            Some(AccountKey("OpenRouter".to_owned())),
            "the selected account is bound for the session's lifetime",
        );
    }

    #[tokio::test]
    async fn an_unknown_org_is_a_loud_404_and_never_reaches_the_upstream() {
        let harness = harness(Duration::ZERO).await;
        let ghost_url = harness.client_url.replacen("/Busytools/", "/GhostOrg/", 1);
        let response = post_as_cli(&format!("{ghost_url}/v1/messages?beta=true")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response.text().await.expect("body");
        assert!(body.contains("GhostOrg"), "the failure names the unknown org, got: {body}");
        assert!(
            harness.requests.lock().is_empty(),
            "an unknown org must not construct an upstream request",
        );
    }

    #[tokio::test]
    async fn a_model_no_account_serves_is_a_loud_503_and_never_reaches_the_upstream() {
        let harness = harness(Duration::ZERO).await;
        // The pin's only account is Anthropic and that account is
        // Ready; a non-claude model has no eligible account in this
        // org. The refusal is the family gate's, not a loading state
        // or an unknown name.
        harness.gateway.set_org_pins([(
            "Busytools".to_owned(),
            crate::selection::OrgPin {
                accounts: vec!["Anthropic".to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("Anthropic".to_owned()), crate::LoadingState::Ready);

        let ghost_url = harness.client_url.replacen("session-1", "ghost", 1);
        let response = reqwest::Client::new()
            .post(format!("{ghost_url}/v1/messages?beta=true"))
            .header(CONTENT_TYPE, "application/json")
            .body(r#"{"model":"glm-5.3-flash","messages":[]}"#)
            .send()
            .await
            .expect("gateway responds");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.expect("body");
        assert!(
            body.contains("glm-5.3-flash") && body.contains("Busytools"),
            "the failure names the model and the org, got: {body}",
        );
        assert!(
            harness.requests.lock().is_empty(),
            "an unselectable request must not construct an upstream request",
        );
    }

    #[tokio::test]
    async fn a_non_claude_model_in_a_non_anthropic_org_binds_and_forwards() {
        let harness = harness(Duration::ZERO).await;
        // Mirror of the family-gate refusal: the org's one account is
        // the OpenRouter one, glm is its family, so the same route
        // that refused above selects, binds, and forwards.
        harness.gateway.set_org_pins([(
            "Busytools".to_owned(),
            crate::selection::OrgPin {
                accounts: vec!["OpenRouter".to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("OpenRouter".to_owned()), crate::LoadingState::Ready);

        let ghost_url = harness.client_url.replacen("session-1", "ghost", 1);
        let response = reqwest::Client::new()
            .post(format!("{ghost_url}/v1/messages?beta=true"))
            .header(CONTENT_TYPE, "application/json")
            .body(r#"{"model":"glm-5.3-flash","messages":[]}"#)
            .send()
            .await
            .expect("gateway responds");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the family gate admits the model to its own family's account",
        );
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "ghost"),
            Some(AccountKey("OpenRouter".to_owned())),
            "the route binds what the family gate admitted",
        );
        assert_eq!(
            harness.requests.lock().len(),
            1,
            "the forwarded request reached the bound account's upstream",
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

    #[tokio::test]
    async fn the_fifth_429_rotates_the_binding_and_the_cooled_account_is_skipped() {
        let harness = harness(Duration::ZERO).await;
        harness.gateway.set_org_pins([(
            "Busytools".to_owned(),
            crate::selection::OrgPin {
                accounts: vec!["OpenRouter".to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
        for _ in 0..4 {
            harness
                .script
                .lock()
                .push_back((429, vec![("retry-after".to_owned(), "30".to_owned())]));
            let response =
                post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        }
        // Four transient 429s: retried in place, the binding holds.
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            Some(AccountKey("OpenRouter".to_owned())),
            "transient 429s do not rotate",
        );
        harness.script.lock().push_back((429, vec![("retry-after".to_owned(), "30".to_owned())]));
        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        // The streak fired: the binding is gone and the account cools,
        // so the CLI's retry cannot come back to it.
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            None,
            "the fifth 429 drops the binding",
        );
        let retry = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(
            retry.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "the cooled account is skipped and nothing else can serve the session",
        );
    }

    #[tokio::test]
    async fn a_rejected_unified_status_header_rotates_on_the_first_response() {
        let harness = harness(Duration::ZERO).await;
        harness.gateway.set_org_pins([(
            "Busytools".to_owned(),
            crate::selection::OrgPin {
                accounts: vec!["OpenRouter".to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
        harness.script.lock().push_back((
            StatusCode::PAYMENT_REQUIRED.as_u16(),
            vec![
                ("retry-after".to_owned(), "45".to_owned()),
                ("anthropic-ratelimit-unified-status".to_owned(), "rejected".to_owned()),
            ],
        ));
        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            None,
            "proven exhaustion drops the binding without waiting for a streak",
        );
        let retry = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(retry.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_success_resets_the_429_streak() {
        let harness = harness(Duration::ZERO).await;
        for _ in 0..4 {
            harness
                .script
                .lock()
                .push_back((429, vec![("retry-after".to_owned(), "30".to_owned())]));
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        }
        // The healthy response lands between the streaks.
        harness.script.lock().push_back((StatusCode::OK.as_u16(), Vec::new()));
        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::OK);
        harness.script.lock().push_back((429, vec![("retry-after".to_owned(), "30".to_owned())]));
        post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            Some(AccountKey("OpenRouter".to_owned())),
            "the streak counts consecutive 429s; one 429 after a success does not rotate",
        );
    }

    #[tokio::test]
    async fn a_rate_limit_report_cools_the_bound_account_until_the_reset() {
        let harness = harness(Duration::ZERO).await;
        harness.gateway.set_org_pins([(
            "Busytools".to_owned(),
            crate::selection::OrgPin {
                accounts: vec!["OpenRouter".to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
        let reset = std::time::SystemTime::now() + Duration::from_secs(120);
        let reset_secs = reset
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("future reset")
            .as_secs();
        harness.gateway.report_rate_limit("session-1", Some(reset_secs));
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            None,
            "the report drops the binding",
        );
        let retry = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(
            retry.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "the account is cooling until the frame's reset time",
        );
        // An unbound session has nothing to rotate and must not panic.
        harness.gateway.report_rate_limit("ghost", None);
    }

    #[tokio::test]
    async fn a_probe_report_cools_the_account_and_unbinds_every_session_on_it() {
        let harness = harness(Duration::ZERO).await;
        harness.gateway.set_org_pins([(
            "Busytools".to_owned(),
            crate::selection::OrgPin {
                accounts: vec!["OpenRouter".to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
        // A second session on the same account, plus the harness's own.
        harness.gateway.bindings.bind(
            "Busytools",
            "forge",
            "session-2",
            AccountKey("OpenRouter".to_owned()),
        );
        let reset = std::time::SystemTime::now() + Duration::from_secs(300);
        let reset_secs = reset
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("future reset")
            .as_secs();
        harness.gateway.report_probe_limit(&AccountKey("OpenRouter".to_owned()), Some(reset_secs));
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            None,
            "the probe's verdict rotates every session on the account",
        );
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-2"),
            None,
            "the probe's verdict rotates every session on the account",
        );
        let retry = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(
            retry.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "the cooled account cannot be re-selected until its reset",
        );
    }

    /// A usage snapshot with the five-hour window at the cap and a
    /// reset ahead: the probe-proven exhaustion shape.
    fn saturated_usage(reset_in: Duration) -> forge_primitives::usage::UsageSnapshot {
        forge_primitives::usage::UsageSnapshot {
            source: forge_primitives::usage::UsageSourceKind::Oauth,
            fetched_at: std::time::SystemTime::UNIX_EPOCH,
            five_hour: Some(forge_primitives::usage::UsageWindow {
                utilization: 100.0,
                resets_at: Some(SystemTime::now() + reset_in),
                reset_description: None,
            }),
            seven_day: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            spend: None,
            balance: None,
        }
    }

    async fn post_model(url: &str, model: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{url}/v1/messages?beta=true"))
            .header(CONTENT_TYPE, "application/json")
            .body(format!(r#"{{"model":"{model}","messages":[]}}"#))
            .send()
            .await
            .expect("gateway responds")
    }

    fn pin_only(harness: &Harness, account: &str) {
        harness.gateway.set_org_pins([(
            "Busytools".to_owned(),
            crate::selection::OrgPin {
                accounts: vec![account.to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
    }

    #[tokio::test]
    async fn the_budget_failure_names_the_accounts_tried_and_the_soonest_reset() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "OpenRouter");
        for _ in 0..5 {
            harness
                .script
                .lock()
                .push_back((429, vec![("retry-after".to_owned(), "30".to_owned())]));
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        }
        // The bound account rotated off on the streak; a fresh session
        // finds the whole walk cooling, which is budget exhaustion, not
        // a family miss.
        let ghost_url = harness.client_url.replacen("session-1", "ghost", 1);
        let response = post_model(&ghost_url, "glm-5.3-flash").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.expect("body");
        assert!(
            body.contains("tried 1 account(s)"),
            "the failure names how many exhausted accounts were tried, got: {body}",
        );
        assert!(
            body.contains("soonest reset in "),
            "the failure names the soonest reset, got: {body}",
        );
    }

    #[tokio::test]
    async fn a_zero_retry_after_falls_through_to_the_probe_reset() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "OpenRouter");
        // The account's own probe reports a reset an hour out; the
        // response carries no usable Retry-After.
        harness.gateway.pool.set_usage(
            &AccountKey("OpenRouter".to_owned()),
            saturated_usage(Duration::from_secs(3600)),
        );
        for _ in 0..5 {
            harness
                .script
                .lock()
                .push_back((429, vec![("retry-after".to_owned(), "0".to_owned())]));
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        }
        let ghost_url = harness.client_url.replacen("session-1", "ghost", 1);
        let response = post_model(&ghost_url, "glm-5.3-flash").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.expect("body");
        assert!(
            body.contains("3599") || body.contains("3600"),
            "the cooldown holds until the probe's reset, not the 60s default, got: {body}",
        );
    }

    #[tokio::test]
    async fn a_rejected_overage_status_header_rotates_on_the_first_response() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "OpenRouter");
        harness.script.lock().push_back((
            StatusCode::PAYMENT_REQUIRED.as_u16(),
            vec![("anthropic-ratelimit-unified-overage-status".to_owned(), "rejected".to_owned())],
        ));
        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            None,
            "the overage arm is proven exhaustion, same as the unified arm",
        );
    }

    #[tokio::test]
    async fn a_zero_reset_in_a_rate_limit_report_still_cools_the_account() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "OpenRouter");
        harness.gateway.report_rate_limit("session-1", Some(0));
        let ghost_url = harness.client_url.replacen("session-1", "ghost", 1);
        let response = post_model(&ghost_url, "glm-5.3-flash").await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a zero reset reads as absent, so the default cooldown still applies"
        );
    }

    #[tokio::test]
    async fn a_model_change_the_bound_account_cannot_serve_rotates() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "Anthropic");
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("Anthropic".to_owned()), crate::LoadingState::Ready);
        // session-1 is bound to OpenRouter; the body now asks for a
        // claude model. The binding rotates to the account that serves
        // it, with the prompt-cache miss accepted.
        let response = post_model(&harness.client_url, "claude-sonnet-5").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            Some(AccountKey("Anthropic".to_owned())),
            "the model change moved the binding to the serving account",
        );
        assert_eq!(harness.requests.lock().len(), 1, "the request was forwarded once");
    }

    #[tokio::test]
    async fn a_model_change_with_no_serving_account_fails_loudly() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "OpenRouter");
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("OpenRouter".to_owned()), crate::LoadingState::Ready);
        // Bound to OpenRouter and asking for claude: the family gate
        // refuses the re-selection instead of forwarding to an account
        // that cannot answer it.
        let response = post_model(&harness.client_url, "claude-sonnet-5").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.expect("body");
        assert!(
            body.contains("claude-sonnet-5") && body.contains("Busytools"),
            "the failure names the model and the org, got: {body}",
        );
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            None,
            "the unservable binding was dropped",
        );
        assert!(
            harness.requests.lock().is_empty(),
            "the request must not reach an account that cannot serve it",
        );
    }

    #[tokio::test]
    async fn a_non_claude_model_on_an_anthropic_binding_rotates_too() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "OpenRouter");
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("OpenRouter".to_owned()), crate::LoadingState::Ready);
        harness.gateway.bindings.bind(
            "Busytools",
            "forge",
            "session-2",
            AccountKey("Anthropic".to_owned()),
        );
        let session_2_url = harness.client_url.replacen("session-1", "session-2", 1);
        let response = post_model(&session_2_url, "glm-5.3-flash").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-2"),
            Some(AccountKey("OpenRouter".to_owned())),
            "the family gate is symmetric across both directions",
        );
    }
}
