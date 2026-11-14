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
//!   attached and the CLI's dummy stripped. A bound session keeps its
//!   account for its whole life: an account the config removes is retired
//!   rather than dropped, and a model the account does not declare is still
//!   forwarded, because the account was the spawn's choice and a running
//!   session is not reconfigured. An unbound session selects an account
//!   now, from the model in its own body.
//! - The failing response streams back to the CLI untouched, but the
//!   triggers it carries mark the account exhausted and rotate the
//!   binding, so the CLI's retry lands on the next account with
//!   nothing replayed.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use hyper::body::Frame;
use hyper::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use hyper::{HeaderMap, Method, Request, StatusCode};

use crate::account::AccountKey;
use crate::binding::{Bindings, DUMMY_CREDENTIAL, credential_variable_for};
use crate::listener::{RouteHandler, StreamBody, text_response};
use crate::rotation::RotationState;
use crate::splice::splice_model;

/// The upstream an account with no base URL of its own talks to: the
/// four Anthropic accounts declare none today, and the CLI's default
/// host is what they have always used.
const ANTHROPIC_UPSTREAM: &str = "https://api.anthropic.com";

/// How long the upstream may produce nothing at all before the
/// forwarded turn is abandoned. An idle bound, not a deadline for the
/// whole response: a turn streams for as long as the model takes, and
/// only silence means the leg is wedged.
pub const FORWARD_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the connect and the TLS handshake may take before the
/// request is abandoned. Well under the idle bound: a host that
/// blackholes the SYN never reaches the first byte, so the idle bound
/// alone would hold the turn open for it, longer than the kernel's own
/// connect timeout did before the leg carried any bound at all.
pub const FORWARD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the upstream may take to send its first content byte after
/// the request is sent. A response that only ever sends SSE comment
/// keepalives is not answering, and [`FORWARD_IDLE_TIMEOUT`] cannot see
/// it: that is a per-read bound, and every comment byte resets it.
pub const FORWARD_FIRST_CONTENT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the upstream may go between content bytes once content has
/// started. Comments are keepalives, not content, so a stream that
/// degrades to comment-only is cut rather than held open forever.
pub const FORWARD_CONTENT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// The error type a streamed body yields.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A forwarded body cut because the upstream stopped answering: it sent
/// no content within the first-content bound, or no content for the
/// content-idle bound after content had started. The CLI sees a cut body
/// and its own retry path engages.
#[derive(Debug, thiserror::Error)]
enum ForwardStall {
    #[error(
        "the upstream sent no content within {bound:?} of the request; only keepalives arrived"
    )]
    FirstContent { bound: Duration },
    #[error(
        "the upstream sent no content for {bound:?} after content started; only keepalives arrived"
    )]
    ContentIdle { bound: Duration },
}

/// The class of the line a response byte belongs to.
#[derive(Clone, Copy, Debug)]
enum LineClass {
    /// An SSE comment line: it starts with `:` and is a keepalive.
    Comment,
    /// A line carrying real response content.
    Content,
    /// A blank line, which separates events and carries nothing.
    Blank,
}

/// Classifies a response byte stream as content or SSE comment
/// keepalives without touching the bytes.
///
/// At the start of the stream and after every `\n`, a line beginning
/// with `:` is a comment, a blank line is neither, and any other line is
/// content. Partial-line state carries across chunk boundaries.
#[derive(Debug)]
struct ContentTracker {
    /// True when the next byte starts a new line.
    at_line_start: bool,
    /// The class of the line currently being read.
    current: Option<LineClass>,
    content_bytes: u64,
    keepalive_bytes: u64,
    total_bytes: u64,
}

impl ContentTracker {
    fn new() -> Self {
        Self {
            at_line_start: true,
            current: None,
            content_bytes: 0,
            keepalive_bytes: 0,
            total_bytes: 0,
        }
    }

    /// Fold one chunk in and answer whether it carried any content byte.
    fn observe(&mut self, bytes: &[u8]) -> bool {
        let mut content_in_chunk = false;
        for &byte in bytes {
            self.total_bytes += 1;
            if self.at_line_start {
                self.current = Some(if byte == b'\n' {
                    LineClass::Blank
                } else if byte == b':' {
                    LineClass::Comment
                } else {
                    LineClass::Content
                });
                self.at_line_start = false;
            }
            match self.current {
                Some(LineClass::Content) => {
                    self.content_bytes += 1;
                    content_in_chunk = true;
                }
                Some(LineClass::Comment) => self.keepalive_bytes += 1,
                Some(LineClass::Blank) | None => {}
            }
            if byte == b'\n' {
                self.at_line_start = true;
                self.current = None;
            }
        }
        content_in_chunk
    }
}

/// The two bounds a forwarded body is held to.
#[derive(Clone, Copy, Debug)]
struct StallBounds {
    first_content: Duration,
    content_idle: Duration,
}

/// Everything a `gateway_forward_stalled` record needs, owned because
/// the body stream outlives the handler's borrows.
struct StallContext {
    account: String,
    org: String,
    project: String,
    session: String,
    model: String,
    upstream: String,
    status: u16,
    request_id: String,
    send_started: tokio::time::Instant,
}

/// A body stream that cuts itself when the upstream stops answering.
///
/// The socket's per-read bound cannot see this: a comment byte resets
/// it, so an upstream that sends `: OPENROUTER PROCESSING` forever never
/// trips anything. Each poll is raced against the applicable deadline -
/// first content from the send, then content-to-content once content has
/// started - and crossing it logs once and ends the body with a
/// [`ForwardStall`].
struct StalledStream<S> {
    inner: S,
    tracker: ContentTracker,
    bounds: StallBounds,
    context: StallContext,
    first_content_deadline: tokio::time::Instant,
    last_content_at: tokio::time::Instant,
    saw_content: bool,
    deadline: tokio::time::Instant,
    /// Kept across polls so a deadline still wakes the task when the
    /// inner stream is silent and produces nothing to poll on.
    sleep: Pin<Box<tokio::time::Sleep>>,
    done: bool,
}

impl<S> StalledStream<S> {
    fn new(inner: S, bounds: StallBounds, context: StallContext) -> Self {
        let first_content_deadline = context.send_started + bounds.first_content;
        Self {
            inner,
            tracker: ContentTracker::new(),
            bounds,
            context,
            first_content_deadline,
            last_content_at: first_content_deadline,
            saw_content: false,
            deadline: first_content_deadline,
            sleep: Box::pin(tokio::time::sleep_until(first_content_deadline)),
            done: false,
        }
    }

    fn emit_stall(&self, phase: &'static str) {
        let context = &self.context;
        tracing::warn!(
            target: "forge_gateway::forward",
            event_name = "gateway_forward_stalled",
            account = %context.account,
            org = %context.org,
            project = %context.project,
            session = %context.session,
            model = %context.model,
            upstream = %context.upstream,
            request_id = %context.request_id,
            status = context.status,
            phase = phase,
            elapsed_ms = context.send_started.elapsed().as_millis(),
            content_bytes = self.tracker.content_bytes,
            keepalive_bytes = self.tracker.keepalive_bytes,
            bytes_total = self.tracker.total_bytes,
            "the forward's upstream opened the response and stopped sending content; the CLI \
             got a cut response it retries",
        );
    }
}

impl<S> Stream for StalledStream<S>
where
    S: Stream<Item = Result<Frame<Bytes>, BoxError>> + Unpin,
{
    type Item = Result<Frame<Bytes>, BoxError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let deadline = if this.saw_content {
            this.last_content_at + this.bounds.content_idle
        } else {
            this.first_content_deadline
        };
        if deadline != this.deadline {
            this.deadline = deadline;
            this.sleep.as_mut().reset(deadline);
        }
        // The inner stream is polled first: a chunk that is already
        // ready is the upstream answering, even at the deadline.
        match this.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref()
                    && this.tracker.observe(data)
                {
                    this.saw_content = true;
                    this.last_content_at = tokio::time::Instant::now();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
            Poll::Ready(None) => {
                this.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    this.done = true;
                    let (phase, error) = if this.saw_content {
                        (
                            "content-idle",
                            ForwardStall::ContentIdle { bound: this.bounds.content_idle },
                        )
                    } else {
                        (
                            "first-content",
                            ForwardStall::FirstContent { bound: this.bounds.first_content },
                        )
                    };
                    this.emit_stall(phase);
                    Poll::Ready(Some(Err(Box::new(error) as BoxError)))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

/// Why a request could not be bound to an account.
#[derive(Debug)]
pub enum SelectFailure {
    /// The org in the path is not in the config.
    UnknownOrg { org: String },
    /// No account in the walk declares the model.
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
    /// `[gateway]` at boot and again on a config reload; the cooldown and
    /// streak maps are the running state and are not replaced by a reload
    /// that leaves the numbers alone.
    rotation: parking_lot::Mutex<RotationState>,
    /// Bumped by every cooldown write that can change what [`Self::select_for`]
    /// answers, so a reader outside this crate can announce the change
    /// instead of holding a roster snapshot the clock has moved past.
    rotation_generation: tokio::sync::watch::Sender<u64>,
    /// How long a response may go without its first content byte.
    first_content_timeout: Duration,
    /// How long content may stop once it has started.
    content_idle_timeout: Duration,
}

impl Gateway {
    /// The caller builds `client` through `ProviderHost`, so the
    /// forward leg carries the TLS trust and the bounds every other
    /// outbound path does:
    /// `host.streaming_http_client(FORWARD_CONNECT_TIMEOUT, FORWARD_IDLE_TIMEOUT)`.
    pub fn new(pool: Arc<crate::AccountPool>, client: reqwest::Client) -> Self {
        Self::with_stall_bounds(
            pool,
            client,
            FORWARD_FIRST_CONTENT_TIMEOUT,
            FORWARD_CONTENT_IDLE_TIMEOUT,
        )
    }

    /// The stall bounds are consts in production; a test drives them
    /// short through this so a stalled upstream is reachable inside one.
    fn with_stall_bounds(
        pool: Arc<crate::AccountPool>,
        client: reqwest::Client,
        first_content_timeout: Duration,
        content_idle_timeout: Duration,
    ) -> Self {
        Self {
            bindings: Bindings::default(),
            pool,
            client,
            org_pins: parking_lot::Mutex::new(HashMap::new()),
            rotation: parking_lot::Mutex::new(RotationState::new()),
            rotation_generation: tokio::sync::watch::channel(0).0,
            first_content_timeout,
            content_idle_timeout,
        }
    }

    /// Subscribe to the rotation's write notifications: every cooldown that
    /// starts (or is replaced) bumps the generation, so a reader wakes and
    /// re-reads what selection would answer.
    pub fn rotation_watch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.rotation_generation.subscribe()
    }

    /// The soonest live cooldown's end, or `None` when nothing is cooling.
    /// A reader sleeps until it to announce the thaw, which the gateway
    /// cannot publish - the clock drives it, not a write.
    pub fn soonest_cooldown_end(&self, now: SystemTime) -> Option<SystemTime> {
        self.rotation.lock().soonest_cooldown_end(now)
    }

    fn note_rotation_changed(&self) {
        self.rotation_generation.send_modify(|generation| *generation += 1);
    }

    /// The rotation numbers from `[gateway]`: the 429 streak count and
    /// window, and the cooldown when no reset time is known. Called at
    /// boot and again by a config reload that moved them.
    pub fn set_rotation_numbers(&self, numbers: crate::rotation::RotationNumbers) {
        *self.rotation.lock() = RotationState::with_numbers(numbers);
        self.note_rotation_changed();
    }

    /// Publish each org's walk order (primary pin then fallbacks).
    /// Called at construction and again by a config reload.
    pub fn set_org_pins(&self, pins: impl IntoIterator<Item = (String, crate::selection::OrgPin)>) {
        *self.org_pins.lock() = pins.into_iter().collect();
    }

    /// The published walk order per org, sorted by org name so a view
    /// renders in a stable order (the store underneath is a HashMap).
    pub fn org_pins(&self) -> Vec<(String, crate::selection::OrgPin)> {
        let mut rows: Vec<(String, crate::selection::OrgPin)> =
            self.org_pins.lock().iter().map(|(org, pin)| (org.clone(), pin.clone())).collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    /// Which account serves `model` for `org`: the org pin lookup, the
    /// cooling filter and the declared-model walk, WITHOUT binding.
    /// This is the one selection decision - the forward binds what it
    /// returns, and a spawn or the launchpad reads it as the answer.
    pub fn select_for(&self, org: &str, model: &str) -> Result<AccountKey, SelectFailure> {
        self.walk(org, model)
    }

    /// The account a spawn lands on first once the walk thaws: among the
    /// cooling accounts declaring `model`, the one whose cooldown ends
    /// soonest. That is the walk's answer at the first moment any of them
    /// is free, which pin order alone does not give - a later-pinned
    /// account can thaw first.
    ///
    /// A prediction made while the walk refuses, not a selection: an
    /// account still `Loading` when the cooldown lifts is skipped by the
    /// walk, so the read can name one that has not settled. A caller uses
    /// it only where `select_for` fails; the walk's own answer replaces it
    /// as soon as the walk succeeds. Selection never calls it.
    pub fn cooling_account_serving(&self, org: &str, model: &str) -> Option<AccountKey> {
        let pin = self.org_pins.lock().get(org).cloned()?;
        let now = SystemTime::now();
        let rotation = self.rotation.lock();
        pin.accounts
            .iter()
            .chain(pin.fallback_accounts.iter())
            .filter_map(|name| {
                let key = AccountKey(name.clone());
                if !rotation.is_cooling_down(&key, now) || !self.pool.declares(&key, model) {
                    return None;
                }
                Some((key.clone(), rotation.cooldown_end(&key)?))
            })
            // Ties keep pin order: `min_by_key` returns the first minimum.
            .min_by_key(|(_, until)| *until)
            .map(|(key, _)| key)
    }

    fn walk(&self, org: &str, model: &str) -> Result<AccountKey, SelectFailure> {
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
        match self.pool.select_account(&pin, org, model) {
            Ok(account) => Ok(account),
            // The walk emptied. When cooling accounts were filtered out,
            // the real cause is exhaustion: they DO serve the model,
            // they are cooling until a knowable time, and the budget
            // failure carries the soonest of THEIR resets. Another org's
            // account clearing sooner is not a time this walk recovers
            // at. A walk empty without cooling is a declared-model miss.
            Err(crate::selection::SelectionError::NoEligibleAccount { model, org }) => {
                let cooled: Vec<AccountKey> = cooled.into_iter().map(AccountKey).collect();
                if let Some(reset) = rotation.soonest_reset_among(&cooled, now) {
                    return Err(SelectFailure::BudgetExhausted {
                        tried: cooled.len(),
                        org,
                        model,
                        reset_in: reset.duration_since(now).unwrap_or(Duration::ZERO),
                    });
                }
                Err(SelectFailure::NoEligibleAccount { model, org })
            }
        }
    }

    /// Select the account for an unbound session and bind it, so the
    /// session keeps it.
    fn select_and_bind(
        &self,
        org: &str,
        project: &str,
        session: &str,
        model: &str,
    ) -> Result<AccountKey, SelectFailure> {
        let account = self.select_for(org, model)?;
        self.bindings.bind(org, project, session, account.clone());
        Ok(account)
    }

    /// Read the real credential and upstream base out of the bound
    /// account's env. `None` when the account carries no credential in
    /// the variable its provider authenticates with.
    fn credential_for(&self, account: &AccountKey) -> Option<(String, String)> {
        let (provider, env) = self.pool.provider_and_env(account)?;
        let variable = credential_variable_for(provider);
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
        let rejected = header_value_is(headers, "anthropic-ratelimit-unified-status", "rejected")
            || header_value_is(headers, "anthropic-ratelimit-unified-overage-status", "rejected");
        let now = SystemTime::now();
        if status == StatusCode::TOO_MANY_REQUESTS && self.rotation.lock().record_429(account, now)
        {
            let cooldown = self.cooldown_for(account, headers);
            self.rotate_off(account, org, project, session, now + cooldown);
        }
        // The header family marks exhaustion on "the failing response"
        // (spec, Rotation): an account not paying for overage reports
        // `-overage-status: rejected` on every response, successes included.
        if rejected && !status.is_success() {
            let cooldown = self.cooldown_for(account, headers);
            self.rotate_off(account, org, project, session, now + cooldown);
        }
        if status.is_success() {
            self.rotation.lock().reset_streak(account);
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
        self.note_rotation_changed();
    }

    /// The workspace's `rate_limit_event` report: the CLI told us the
    /// bound account's window is not `allowed`, and the frame's reset
    /// time is the cooldown. Both the exhausted account and the probe
    /// agree on a reset time or its absence; nothing else is read.
    /// A session with no binding (gateway not in use) has nothing to
    /// rotate.
    pub fn report_rate_limit(
        &self,
        org: &str,
        project: &str,
        session: &str,
        reset_at: Option<u64>,
    ) {
        let Some(account) = self.bindings.take_binding(org, project, session) else {
            return;
        };
        let now = SystemTime::now();
        // A zero reset means no usable reset: it would end the cooldown
        // the instant it starts, so the configured default applies.
        let until = reset_at
            .filter(|secs| *secs > 0)
            .map_or(now + self.rotation.lock().no_reset_cooldown(), crate::rotation::reset_instant);
        self.rotation.lock().cool_down(&account, until);
        self.note_rotation_changed();
    }

    /// Whether the rotation is holding `account` out of selection right now -
    /// the cooldown a probe at a window's cap writes.
    ///
    /// A view reads it to state the selection signal without stating the
    /// usage: a probe landing at the cap moves this and nothing on the
    /// account's loading row.
    pub fn is_cooling(&self, account: &AccountKey) -> bool {
        self.rotation.lock().is_cooling_down(account, SystemTime::now())
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
        self.note_rotation_changed();
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

/// How deep a cause chain is walked before it is declared pathological.
/// Real chains from reqwest + hyper are a handful of layers; a custom
/// `source()` that returns itself must not turn a log site into a hang.
const MAX_ERROR_CHAIN_DEPTH: usize = 16;

/// The chain's layers, top first. The bound counts layers visited, not
/// layers kept, so a self-referential `source()` terminates even when every
/// layer renders the same text.
fn error_chain<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> (Vec<&'a (dyn std::error::Error + 'static)>, bool) {
    let mut layers = vec![error];
    let mut source = error.source();
    let mut truncated = false;
    while let Some(cause) = source {
        if layers.len() >= MAX_ERROR_CHAIN_DEPTH {
            truncated = true;
            break;
        }
        layers.push(cause);
        source = cause.source();
    }
    (layers, truncated)
}

/// The whole cause chain of a failed forward leg on one line,
/// `top -> ... -> root`.
///
/// reqwest's own `Display` is the top layer only, and every actionable fact
/// (DNS, connect, TLS, reset, EMFILE) sits in a `source()` below it, so the
/// top alone cannot tell the failures apart. The leg's two failure sites
/// open differently: a failed send starts at "error sending request for url
/// (...)", while a body that fails after the response started starts at
/// "error decoding response body".
fn describe_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let (layers, truncated) = error_chain(error);
    let mut parts: Vec<String> = Vec::new();
    for layer in layers {
        let text = layer.to_string();
        // A transparent wrapper renders its cause's text; keeping one copy
        // and walking past it is what still reaches the root.
        if parts.last().is_some_and(|last| *last == text) {
            continue;
        }
        parts.push(text);
    }
    // Without the marker a capped chain ends mid-walk and its last line
    // reads as the root.
    if truncated {
        parts.push(format!("... (truncated at {MAX_ERROR_CHAIN_DEPTH} layers)"));
    }
    parts.join(" -> ")
}

/// The transport-level shape of a failed forward leg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForwardFailureKind {
    Dns,
    Connect,
    Tls,
    Reset,
    Closed,
    Timeout,
    Fd,
    Http2,
    Other,
}

impl ForwardFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Reset => "reset",
            Self::Closed => "closed",
            Self::Timeout => "timeout",
            Self::Fd => "fd",
            Self::Http2 => "http2",
            Self::Other => "other",
        }
    }
}

/// Read the transport failure's kind out of the whole chain.
///
/// String matching rather than downcasts: the layer that carries the cause
/// is a different crate's error type on every reqwest release, and matching
/// words keeps one mechanism for the kinds reqwest has no predicate for
/// (tls, reset, closed, fd, http2). Precedence runs most specific first -
/// EMFILE is a connect failure by position but its own problem by cause, and
/// both "connection reset by peer" and "connection closed" contain
/// "connect".
///
/// The top layer is skipped when a cause sits below it: reqwest's send top
/// `Display` embeds the request URL, so a host carrying "ssl" or "dns"
/// would otherwise decide the kind, and the stream top ("error decoding
/// response body") names no cause either. A flat error has no wrapper, so
/// its own text is still read.
fn classify_forward_failure(error: &(dyn std::error::Error + 'static)) -> ForwardFailureKind {
    const NEEDLES: &[(ForwardFailureKind, &[&str])] = &[
        (ForwardFailureKind::Fd, &["too many open files", "os error 24", "emfile"]),
        (
            ForwardFailureKind::Dns,
            &[
                "dns",
                "lookup address",
                "name or service not known",
                "name resolution",
                "nodename nor servname",
            ],
        ),
        (ForwardFailureKind::Tls, &["tls", "ssl", "certificate"]),
        (ForwardFailureKind::Timeout, &["timed out", "timeout"]),
        (ForwardFailureKind::Reset, &["reset"]),
        (ForwardFailureKind::Closed, &["closed", "broken pipe", "eof"]),
        (ForwardFailureKind::Connect, &["connect", "refused", "unreachable", "network is down"]),
        // Last, because h2's own words sit beside causes the kinds above
        // already answer for: a peer close below the wrapper stays
        // "closed", and REFUSED_STREAM's "refused" stays "connect".
        (ForwardFailureKind::Http2, &["http2 error", "stream error", "unspecific protocol error"]),
    ];
    let (layers, _) = error_chain(error);
    let layers: Vec<String> =
        layers.iter().map(|layer| layer.to_string().to_ascii_lowercase()).collect();
    let candidates = if layers.len() > 1 { &layers[1..] } else { &layers[..] };
    for (kind, needles) in NEEDLES {
        if candidates.iter().any(|layer| needles.iter().any(|needle| layer.contains(needle))) {
            return *kind;
        }
    }
    ForwardFailureKind::Other
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

        // A bound session keeps its account: a model change is forwarded
        // on the account the session spawned under, because that choice
        // was the spawn's and a running session is not reconfigured. An
        // unbound session selects now, from the model in its own body and
        // the org named in its path; the selection failure names both.
        let account = match self.bindings.binding_for(org, project, session) {
            Some(bound) => bound,
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

        // The body carries the canonical model name; the upstream may
        // spell it differently. When the account declares a slug for
        // the model, splice the slug in - never deserializing.
        let spliced = match self.pool.model_slug_for(&account, &model) {
            Some(slug) => {
                splice_model(&spliced, Some(&slug)).map(|(body, _)| body).unwrap_or(spliced)
            }
            None => spliced,
        };

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

        let send_started = tokio::time::Instant::now();
        let upstream_response = match self
            .client
            .post(url.as_str())
            .headers(outbound_headers)
            .header(hyper::header::AUTHORIZATION, format!("Bearer {credential}"))
            .body(spliced)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                // The CLI's retry loop has no cause to act on: its frame
                // carries this body and nothing else. The chain is the only
                // place DNS, connect, TLS, reset and EMFILE differ, and this
                // is forge's own problem rather than the session's, so it
                // logs at warn with everything a reader needs to act.
                let kind = classify_forward_failure(&error);
                let chain = describe_error_chain(&error);
                tracing::warn!(
                    target: "forge_gateway::forward",
                    event_name = "gateway_forward_failed",
                    account = %account.0,
                    org = %org,
                    project = %project,
                    session = %session,
                    model = %model,
                    upstream = %url,
                    kind = kind.as_str(),
                    elapsed_ms = send_started.elapsed().as_millis(),
                    error_chain = %chain,
                    "the forward's upstream send failed; the 502 the CLI retries carries no cause",
                );
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
        // The head is already on the wire, so a body failure can only be a
        // cut to the CLI - there is no status left to change and nothing
        // left to rewrite. What the CLI retries with is a truncated stream
        // carrying no cause, and the chain is the only place a reset, a
        // timeout and an upstream that died mid-turn differ. The leg is
        // forge's own rather than the session's work, so it logs at warn
        // with everything a reader needs to act, the provider's request id
        // included: that id is the correlation key to raise the cut with
        // the provider.
        let stream_started = std::time::Instant::now();
        let stream_account = account.0;
        let stream_org = org.to_owned();
        let stream_project = project.to_owned();
        let stream_session = session.to_owned();
        let stream_model = model;
        let stream_url = url;
        let stall_context = StallContext {
            account: stream_account.clone(),
            org: stream_org.clone(),
            project: stream_project.clone(),
            session: stream_session.clone(),
            model: stream_model.clone(),
            upstream: stream_url.clone(),
            status: status.as_u16(),
            request_id: response_headers
                .get("request-id")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
            send_started,
        };
        let stream_headers = response_headers;
        let mapped = upstream_response.bytes_stream().map(move |chunk| {
            chunk.map(Frame::data).map_err(|error| {
                let kind = classify_forward_failure(&error);
                let chain = describe_error_chain(&error);
                let request_id = stream_headers
                    .get("request-id")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                tracing::warn!(
                    target: "forge_gateway::forward",
                    event_name = "gateway_forward_stream_failed",
                    account = %stream_account,
                    org = %stream_org,
                    project = %stream_project,
                    session = %stream_session,
                    model = %stream_model,
                    upstream = %stream_url,
                    request_id = %request_id,
                    status = status.as_u16(),
                    kind = kind.as_str(),
                    stream_elapsed_ms = stream_started.elapsed().as_millis(),
                    error_chain = %chain,
                    "the forward's upstream body failed after the response started; the CLI got a cut response",
                );
                Box::new(error) as BoxError
            })
        });
        let stream: Pin<Box<dyn Stream<Item = Result<Frame<Bytes>, BoxError>> + Send + Sync>> =
            Box::pin(StalledStream::new(
                Box::pin(mapped),
                StallBounds {
                    first_content: self.first_content_timeout,
                    content_idle: self.content_idle_timeout,
                },
                stall_context,
            ));
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

    /// What the stub's body does after its head.
    #[derive(Clone, Copy, Debug)]
    enum StubBody {
        /// The healthy shape: one more chunk after the gap, then the end.
        Complete,
        /// An upstream that dies mid-response: the head and the first
        /// chunk are on the wire, then the body aborts.
        Cut,
        /// Only SSE comment keepalives, forever: an upstream that opened
        /// the response and never answers. Comment lines are split across
        /// chunks so partial-line state is exercised.
        CommentsForever,
        /// One real data chunk, then comment keepalives forever.
        ContentThenCommentsForever,
        /// Comments interleaved between real deltas, then a clean end.
        HealthyWithComments,
        /// A plain JSON body split across a line, no SSE framing.
        PlainJson,
    }

    /// A stub upstream that records what arrived and streams the body
    /// [`StubBody`] describes, with chunks separated by `chunk_delay`, so
    /// a buffering gateway is measurable from the client side.
    struct RecordingUpstream {
        requests: Arc<parking_lot::Mutex<Vec<Recorded>>>,
        chunk_delay: Duration,
        /// The shape of the body the stub streams back.
        body: StubBody,
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
            let body = self.body;
            let stream = futures_util::stream::unfold(
                (0usize, delay, body),
                move |state: (usize, Duration, StubBody)| async move {
                    let (step, delay, body) = state;
                    let frame = |bytes: &'static [u8]| {
                        Ok::<Frame<Bytes>, BoxError>(Frame::data(Bytes::from_static(bytes)))
                    };
                    match body {
                        StubBody::Complete => match step {
                            0 => Some((frame(b"event: first\n"), (1, delay, body))),
                            1 => {
                                tokio::time::sleep(delay).await;
                                Some((frame(b"event: second\n"), (2, delay, body)))
                            }
                            _ => None,
                        },
                        StubBody::Cut => match step {
                            0 => Some((frame(b"event: first\n"), (1, delay, body))),
                            1 => {
                                tokio::time::sleep(delay).await;
                                Some((
                                    Err(Box::new(std::io::Error::new(
                                        std::io::ErrorKind::ConnectionReset,
                                        "stub upstream died mid-response",
                                    )) as BoxError),
                                    (2, delay, body),
                                ))
                            }
                            _ => None,
                        },
                        StubBody::CommentsForever => {
                            tokio::time::sleep(delay).await;
                            let bytes: &'static [u8] =
                                if step % 2 == 0 { b": OPENROUTER " } else { b"PROCESSING\n" };
                            Some((frame(bytes), (step + 1, delay, body)))
                        }
                        StubBody::ContentThenCommentsForever => {
                            if step == 0 {
                                Some((frame(b"data: hello\n\n"), (1, delay, body)))
                            } else {
                                tokio::time::sleep(delay).await;
                                Some((frame(b": keepalive\n"), (step + 1, delay, body)))
                            }
                        }
                        StubBody::HealthyWithComments => {
                            if step >= 12 {
                                None
                            } else {
                                tokio::time::sleep(delay).await;
                                let bytes: &'static [u8] = if step % 2 == 0 {
                                    b": keepalive\n"
                                } else {
                                    b"data: delta\n\n"
                                };
                                Some((frame(bytes), (step + 1, delay, body)))
                            }
                        }
                        StubBody::PlainJson => match step {
                            0 => Some((frame(b"{\"ok\":"), (1, delay, body))),
                            1 => {
                                tokio::time::sleep(delay).await;
                                Some((frame(b"true}"), (2, delay, body)))
                            }
                            _ => None,
                        },
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
        /// The stub upstream, so a test that rewrites the pool's env can
        /// keep the request off the real network.
        upstream_url: String,
        gateway: Arc<Gateway>,
        requests: Arc<parking_lot::Mutex<Vec<Recorded>>>,
        script: ScriptHandle,
    }

    /// The host the harness builds its forward client through, so the
    /// leg runs against the port's own construction rather than a
    /// hand-built client.
    struct TestHost;

    #[async_trait::async_trait]
    impl crate::ProviderHost for TestHost {
        fn http_client(&self, _timeout: Duration) -> Result<reqwest::Client, String> {
            unreachable!("the forward harness runs no probe")
        }

        fn streaming_http_client(
            &self,
            connect_timeout: Duration,
            idle_timeout: Duration,
        ) -> Result<reqwest::Client, String> {
            reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .read_timeout(idle_timeout)
                .build()
                .map_err(|e| e.to_string())
        }

        async fn user_agent(&self) -> Result<String, String> {
            unreachable!("the forward harness runs no probe")
        }
    }

    /// Stub upstream + gateway listener + one registered session whose
    /// account's upstream is the stub. The pool holds one account per
    /// provider, both with the stub as their upstream, so a test that
    /// asserts "no upstream request" is airtight whichever account a
    /// broken selection could have picked.
    async fn harness(chunk_delay: Duration) -> Harness {
        harness_with(
            chunk_delay,
            FORWARD_IDLE_TIMEOUT,
            FORWARD_FIRST_CONTENT_TIMEOUT,
            FORWARD_CONTENT_IDLE_TIMEOUT,
            StubBody::Complete,
        )
        .await
    }

    /// [`harness`] with the forward leg's per-read idle bound under the
    /// test's control, so a stalled upstream is reachable inside a test.
    async fn harness_with_idle_timeout(chunk_delay: Duration, idle_timeout: Duration) -> Harness {
        harness_with(
            chunk_delay,
            idle_timeout,
            FORWARD_FIRST_CONTENT_TIMEOUT,
            FORWARD_CONTENT_IDLE_TIMEOUT,
            StubBody::Complete,
        )
        .await
    }

    /// [`harness`] with the stub's body shape under the test's control,
    /// so an upstream that dies mid-response or only ever sends comments
    /// is reachable inside a test.
    async fn harness_with_tail(chunk_delay: Duration, body: StubBody) -> Harness {
        harness_with(
            chunk_delay,
            FORWARD_IDLE_TIMEOUT,
            FORWARD_FIRST_CONTENT_TIMEOUT,
            FORWARD_CONTENT_IDLE_TIMEOUT,
            body,
        )
        .await
    }

    /// [`harness`] with both stall bounds and the body shape under the
    /// test's control, so an upstream that opens a response and only ever
    /// sends comments is reachable inside a test.
    async fn harness_with_stall_bounds(
        chunk_delay: Duration,
        idle_timeout: Duration,
        first_content_timeout: Duration,
        content_idle_timeout: Duration,
        body: StubBody,
    ) -> Harness {
        harness_with(chunk_delay, idle_timeout, first_content_timeout, content_idle_timeout, body)
            .await
    }

    async fn harness_with(
        chunk_delay: Duration,
        idle_timeout: Duration,
        first_content_timeout: Duration,
        content_idle_timeout: Duration,
        body: StubBody,
    ) -> Harness {
        let script: ScriptHandle =
            Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let upstream = Arc::new(RecordingUpstream {
            requests: Arc::clone(&requests),
            chunk_delay,
            body,
            script: Arc::clone(&script),
        });
        let stub_port = free_port().await;
        let stub_listener = GatewayListener::bind(stub_port).await.expect("stub bind");
        let stub_url = format!("http://127.0.0.1:{stub_port}");
        let upstream_url = stub_url.clone();
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
                provider: forge_primitives::account::Provider::Openrouter,
                base_url: None,
                models: vec!["glm-5.3-flash".to_owned()],
                model_aliases: std::collections::HashMap::new(),
                model_slugs: std::collections::HashMap::from([(
                    "glm-5.3-flash".to_owned(),
                    "zai-org/glm-5.3-flash".to_owned(),
                )]),
                env: account_env.clone(),
            },
            forge_primitives::account::LoadedAccount {
                display_name: "Anthropic".to_owned(),
                provider: forge_primitives::account::Provider::Anthropic,
                base_url: None,
                models: vec!["claude-sonnet-5".to_owned(), "claude-opus-5".to_owned()],
                model_aliases: std::collections::HashMap::new(),
                model_slugs: std::collections::HashMap::new(),
                env: anthropic_env,
            },
        ]));
        let client = crate::ProviderHost::streaming_http_client(
            &TestHost,
            FORWARD_CONNECT_TIMEOUT,
            idle_timeout,
        )
        .expect("the test host builds a client");
        let gateway = Arc::new(Gateway::with_stall_bounds(
            Arc::clone(&pool),
            client,
            first_content_timeout,
            content_idle_timeout,
        ));

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

        Harness { client_url, upstream_url, gateway, requests, script }
    }

    /// A CLI-shaped POST: the dummy credential in the auth variable's
    /// header form, a junk `x-api-key`, and the entitlement headers the
    /// real CLI emits - exactly what a launching env with
    /// `ANTHROPIC_API_KEY` set would produce.
    async fn post_as_cli(url: &str) -> reqwest::Response {
        // The harness session is bound to the OpenRouter account, so
        // the body carries a model that account declares: a claude
        // model here would rotate the binding instead of forwarding.
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
        // org. The refusal is the declared-model gate's, not a loading
        // state or an unknown name.
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
        // Mirror of the refusal above: the org's one account is the
        // OpenRouter one, glm is in its declared models, so the same
        // route selects, binds, and forwards.
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
            "the model gate admits the model to a declaring account",
        );
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "ghost"),
            Some(AccountKey("OpenRouter".to_owned())),
            "the route binds what the declared-model gate admitted",
        );
        assert_eq!(
            harness.requests.lock().len(),
            1,
            "the forwarded request reached the bound account's upstream",
        );
    }

    #[tokio::test]
    async fn the_response_streams_while_the_upstream_is_still_sending() {
        use tracing_subscriber::layer::SubscriberExt;

        let harness = harness(Duration::from_millis(1000)).await;
        let caught = Caught::default();
        let subscriber = tracing_subscriber::Registry::default().with(caught.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
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
        let end = response.chunk().await.expect("the body ends cleanly");
        assert!(end.is_none(), "the stub's body completes rather than cutting");

        // A completed stream is not a failure: the other arm's record stays
        // silent, so a healthy turn's log is uninterrupted by this site.
        let caught = caught.0.lock().expect("capture").clone();
        assert!(
            caught.iter().all(|(_, target, _)| *target != "forge_gateway::forward"),
            "a healthy forward emits no failure record: {caught:?}",
        );
    }

    /// The connect bound is the tighter inner one, and that ordering is
    /// its whole reason for existing: it is what a host that never
    /// produces a first byte runs into, where the idle bound would hold
    /// the turn open far longer than a connect ever should.
    #[test]
    fn the_connect_bound_stays_well_under_the_idle_bound() {
        assert!(
            FORWARD_CONNECT_TIMEOUT <= FORWARD_IDLE_TIMEOUT / 10,
            "the connect bound is the tighter one; {FORWARD_CONNECT_TIMEOUT:?} against an idle \
             bound of {FORWARD_IDLE_TIMEOUT:?} lets a blackholed connect outlast the wedge the \
             leg is bounded to stop",
        );
    }

    /// The forward leg's client is the host's, and a host that bounds
    /// idle reads ends a wedged upstream: the CLI's turn is abandoned
    /// instead of held open until the upstream produces anything.
    #[tokio::test]
    async fn a_stalled_upstream_ends_the_forwarded_stream_at_the_hosts_idle_bound() {
        let harness =
            harness_with_idle_timeout(Duration::from_secs(5), Duration::from_millis(200)).await;
        let started = Instant::now();
        let mut response =
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;

        let first = response.chunk().await.expect("a body chunk").expect("chunk bytes");
        assert!(!first.is_empty(), "the first chunk arrives while the upstream is healthy");

        let stalled = tokio::time::timeout(Duration::from_secs(8), response.chunk())
            .await
            .expect("the idle bound ends the stream; an unbounded client never returns here");
        assert!(
            stalled.is_err(),
            "the stream is cut mid-body when the upstream goes quiet; got {stalled:?}",
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the host's bound ended the stream, not the upstream's next chunk; took {:?}",
            started.elapsed(),
        );
    }

    /// Read a body to its end, answering the bytes read and whether the
    /// read was cut. A cut is what the CLI's retry path reacts to.
    async fn read_body(response: &mut reqwest::Response) -> (Vec<u8>, bool) {
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
                Ok(None) => return (bytes, false),
                Err(_) => return (bytes, true),
            }
        }
    }

    /// Every `forge_gateway::forward` record the capture holds.
    fn forward_records(caught: &Caught) -> Vec<(tracing::Level, String)> {
        caught
            .0
            .lock()
            .expect("capture")
            .iter()
            .filter(|(_, target, _)| *target == "forge_gateway::forward")
            .map(|(level, _, fields)| (*level, fields.clone()))
            .collect()
    }

    /// One `name=value` field out of a captured record.
    fn field_u64(fields: &str, name: &str) -> u64 {
        let marker = format!("{name}=");
        fields
            .split(&marker)
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("{name} missing from {fields}"))
    }

    /// The line this change exists for: an upstream that opens the
    /// response and then writes only SSE comment keepalives keeps the
    /// socket's per-read timer alive forever, so nothing else in the leg
    /// ever sees the stall. The first-content bound cuts it, logs once
    /// with the phase and the byte counts, and the CLI's body read sees a
    /// cut it retries.
    #[tokio::test]
    async fn a_response_that_only_sends_comments_stalls_at_the_first_content_bound() {
        use tracing_subscriber::layer::SubscriberExt;

        let harness = harness_with_stall_bounds(
            Duration::from_millis(40),
            Duration::from_secs(5),
            Duration::from_millis(200),
            Duration::from_secs(5),
            StubBody::CommentsForever,
        )
        .await;
        let caught = Caught::default();
        let subscriber = tracing_subscriber::Registry::default().with(caught.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let started = Instant::now();
        let mut response =
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::OK, "the head reached the CLI before the stall");

        let (_, cut) = tokio::time::timeout(Duration::from_secs(5), read_body(&mut response))
            .await
            .expect("the first-content bound ends the stream; an unbounded client never returns");
        assert!(cut, "the stream is cut, not completed: comments are not an answer");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the bound ended the stream, not the test's own timeout; took {:?}",
            started.elapsed(),
        );

        let records = forward_records(&caught);
        assert_eq!(records.len(), 1, "one stall emits exactly one record: {records:?}");
        let (level, fields) = &records[0];
        assert_eq!(*level, tracing::Level::WARN, "the line claims forge's own problem: {fields}");
        for needle in [
            "event_name=\"gateway_forward_stalled\"",
            "account=OpenRouter",
            "org=Busytools",
            "project=forge",
            "session=session-1",
            "model=glm-5.3-flash",
            &format!("upstream={}/v1/messages?beta=true", harness.upstream_url),
            "request_id=req-test",
            "status=200",
            "phase=\"first-content\"",
            "elapsed_ms=",
            "content_bytes=0",
            "keepalive_bytes=",
            "bytes_total=",
        ] {
            assert!(fields.contains(needle), "{needle} missing from {fields}");
        }
        assert!(
            field_u64(fields, "keepalive_bytes") > 0,
            "the comments really did arrive: {fields}",
        );
        assert!(!fields.contains("real-openrouter-key"), "no credential in the record: {fields}");
    }

    /// The other shape: the upstream answered, then degraded to comments.
    /// Content had started, so the content-idle bound is what cuts it, and
    /// the record says content was seen.
    #[tokio::test]
    async fn a_response_that_degrades_to_comments_after_content_stalls_at_the_content_idle_bound() {
        use tracing_subscriber::layer::SubscriberExt;

        let harness = harness_with_stall_bounds(
            Duration::from_millis(40),
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_millis(200),
            StubBody::ContentThenCommentsForever,
        )
        .await;
        let caught = Caught::default();
        let subscriber = tracing_subscriber::Registry::default().with(caught.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut response =
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::OK);

        let (body, cut) = tokio::time::timeout(Duration::from_secs(5), read_body(&mut response))
            .await
            .expect("the content-idle bound ends the stream; an unbounded client never returns");
        assert!(cut, "the stream is cut once content stops");
        assert!(
            body.starts_with(b"data: hello\n\n"),
            "the real content reached the CLI before the stall: {}",
            String::from_utf8_lossy(&body),
        );

        let records = forward_records(&caught);
        assert_eq!(records.len(), 1, "one stall emits exactly one record: {records:?}");
        let (level, fields) = &records[0];
        assert_eq!(*level, tracing::Level::WARN, "the line claims forge's own problem: {fields}");
        for needle in [
            "event_name=\"gateway_forward_stalled\"",
            "account=OpenRouter",
            "phase=\"content-idle\"",
            "request_id=req-test",
            "status=200",
            "elapsed_ms=",
            "keepalive_bytes=",
            "bytes_total=",
        ] {
            assert!(fields.contains(needle), "{needle} missing from {fields}");
        }
        assert!(field_u64(fields, "content_bytes") > 0, "content had started: {fields}");
        assert!(field_u64(fields, "keepalive_bytes") > 0, "comments followed: {fields}");
    }

    /// Comments between real deltas are keepalives, not a stall. The
    /// first-content bound is tighter than the body's own length, so a
    /// tracker that misreads content as a keepalive cuts a healthy stream
    /// here.
    #[tokio::test]
    async fn a_healthy_stream_with_interleaved_comments_completes_without_a_stall() {
        use tracing_subscriber::layer::SubscriberExt;

        let harness = harness_with_stall_bounds(
            Duration::from_millis(40),
            Duration::from_secs(5),
            Duration::from_millis(200),
            Duration::from_millis(300),
            StubBody::HealthyWithComments,
        )
        .await;
        let caught = Caught::default();
        let subscriber = tracing_subscriber::Registry::default().with(caught.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut response =
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::OK);

        let (body, cut) = tokio::time::timeout(Duration::from_secs(5), read_body(&mut response))
            .await
            .expect("a healthy stream with comments completes");
        assert!(!cut, "comments between deltas are keepalives, not a stall");
        let mut expected = Vec::new();
        for _ in 0..6 {
            expected.extend_from_slice(b": keepalive\n");
            expected.extend_from_slice(b"data: delta\n\n");
        }
        assert_eq!(body, expected, "every byte, comments included, reaches the CLI unchanged");
        let records = forward_records(&caught);
        assert!(records.is_empty(), "a healthy stream emits no forward record: {records:?}");
    }

    /// A non-streaming body - an upstream's JSON error, say - carries no
    /// SSE framing at all. Its bytes are content, so the first-content
    /// bound must not fire on it. The body outlives that bound and its
    /// line is split across the gap, so a tracker that only counts `data:`
    /// lines would cut it.
    #[tokio::test]
    async fn a_plain_json_body_counts_as_content_and_is_not_stalled() {
        use tracing_subscriber::layer::SubscriberExt;

        let harness = harness_with_stall_bounds(
            Duration::from_millis(300),
            Duration::from_secs(5),
            Duration::from_millis(200),
            Duration::from_secs(5),
            StubBody::PlainJson,
        )
        .await;
        let caught = Caught::default();
        let subscriber = tracing_subscriber::Registry::default().with(caught.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut response =
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::OK);

        let (body, cut) = tokio::time::timeout(Duration::from_secs(5), read_body(&mut response))
            .await
            .expect("the plain JSON body completes");
        assert!(!cut, "a plain JSON body is an answer, not a stall");
        assert_eq!(String::from_utf8_lossy(&body), r#"{"ok":true}"#);
        let records = forward_records(&caught);
        assert!(records.is_empty(), "a plain JSON body emits no stall record: {records:?}");
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

    /// The spec scopes the header trigger to `rejected` "on the failing
    /// response" (spec, Rotation), so a 200 carrying it cools nothing.
    /// The overage arm is the one that bites: an account not paying for
    /// overage reports `overage-status: rejected` on every response it
    /// sends, successful ones included.
    #[tokio::test]
    async fn a_success_carrying_rejected_cools_nothing() {
        for header in
            ["anthropic-ratelimit-unified-status", "anthropic-ratelimit-unified-overage-status"]
        {
            let harness = harness(Duration::ZERO).await;
            harness.script.lock().push_back((
                StatusCode::OK.as_u16(),
                vec![(header.to_owned(), "rejected".to_owned())],
            ));
            let response =
                post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
            assert_eq!(response.status(), StatusCode::OK, "the status streams back untouched");
            assert_eq!(
                harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
                Some(AccountKey("OpenRouter".to_owned())),
                "{header}: a success is not a failing response, so the binding survives",
            );
            assert!(
                !harness
                    .gateway
                    .rotation
                    .lock()
                    .is_cooling_down(&AccountKey("OpenRouter".to_owned()), SystemTime::now()),
                "{header}: a success must not start a cooldown",
            );
        }
    }

    /// A rejected FAILING response is not a success: it rotates without
    /// clearing the streak on its way past. `retry-after: 0` reads as no
    /// reset time and a zero no-reset cooldown cools the account to the
    /// instant, so the rotated account is selectable again at once and
    /// the streak alone decides whether the next 429 is the fifth.
    #[tokio::test]
    async fn a_rejected_failing_response_does_not_clear_the_429_streak() {
        let harness = harness(Duration::ZERO).await;
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
        harness.gateway.set_rotation_numbers(crate::rotation::RotationNumbers {
            no_reset_cooldown: Duration::ZERO,
            ..Default::default()
        });
        for _ in 0..4 {
            harness
                .script
                .lock()
                .push_back((429, vec![("retry-after".to_owned(), "30".to_owned())]));
            let response =
                post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "the streak is counting");
        }
        harness.script.lock().push_back((
            StatusCode::PAYMENT_REQUIRED.as_u16(),
            vec![
                ("anthropic-ratelimit-unified-status".to_owned(), "rejected".to_owned()),
                ("retry-after".to_owned(), "0".to_owned()),
            ],
        ));
        post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            None,
            "the 402's rejected header rotated the binding, not a streak",
        );
        harness.script.lock().push_back((429, vec![("retry-after".to_owned(), "30".to_owned())]));
        let fifth = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(
            fifth.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the fifth 429 reached the upstream, so the binding below is the streak's verdict",
        );
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            None,
            "the rejection left the four 429s standing, so this one is the fifth",
        );
    }

    /// A 429 carrying the standing `-overage-status: rejected` rotates
    /// below the streak threshold: the header arm reads the status and a
    /// 429 is not a success. That is newer than pre-regression, where the
    /// 429 arm was exclusive, so it is pinned rather than fixed. The
    /// standing-state distinction (`-overage-disabled-reason: out_of_credits`)
    /// is the architectural option if it ever needs one.
    #[tokio::test]
    async fn a_429_carrying_the_standing_rejection_rotates_below_the_streak() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "OpenRouter");
        harness.script.lock().push_back((
            429,
            vec![("anthropic-ratelimit-unified-overage-status".to_owned(), "rejected".to_owned())],
        ));
        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            None,
            "one 429 is not a streak, so the header arm is what rotated it",
        );
    }

    /// The spec: "any success resets it". The success here carries the
    /// standing overage rejection, which is what an account not paying
    /// for overage sends on every response, so the reset has to survive
    /// that header to be reachable at all.
    #[tokio::test]
    async fn a_success_carrying_rejected_resets_the_429_streak() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "OpenRouter");
        for _ in 0..4 {
            harness
                .script
                .lock()
                .push_back((429, vec![("retry-after".to_owned(), "30".to_owned())]));
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        }
        // The healthy response lands between the streaks.
        harness.script.lock().push_back((
            StatusCode::OK.as_u16(),
            vec![("anthropic-ratelimit-unified-overage-status".to_owned(), "rejected".to_owned())],
        ));
        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::OK);
        harness.script.lock().push_back((429, vec![("retry-after".to_owned(), "30".to_owned())]));
        post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            Some(AccountKey("OpenRouter".to_owned())),
            "the streak counts consecutive 429s; one after a success does not rotate",
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
        harness.gateway.report_rate_limit("Busytools", "forge", "session-1", Some(reset_secs));
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
        harness.gateway.report_rate_limit("Busytools", "forge", "ghost", None);
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
                status: None,
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

    /// The forward leg reads a native account's credential from the
    /// variable the stamp put its dummy in - the same rule the stamper
    /// and the loader call, so the reader cannot drift to the other
    /// variable and 503 every turn.
    #[tokio::test]
    async fn the_forward_leg_reads_a_native_accounts_credential_variable() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "Anthropic");
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("Anthropic".to_owned()), crate::LoadingState::Ready);
        harness.gateway.bindings.bind(
            "Busytools",
            "forge",
            "session-1",
            AccountKey("Anthropic".to_owned()),
        );

        let response = post_model(&harness.client_url, "claude-sonnet-5").await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the credential is found in the account's own variable; reading the other one 503s",
        );
        let requests = harness.requests.lock();
        assert_eq!(
            requests[0].first("authorization"),
            Some("Bearer real-oauth-token"),
            "the real credential is read back out of the account's own variable",
        );
    }

    /// The account's declared slug replaces the canonical model name
    /// on the forwarded body - the upstream spelling is the account's
    /// business, invisible to the CLI.
    #[tokio::test]
    async fn a_declared_slug_replaces_the_canonical_model_upstream() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "OpenRouter");
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("OpenRouter".to_owned()), crate::LoadingState::Ready);
        // Give the account a slug for the canonical model: the forward
        // must splice the slug in, not the name the CLI sent.
        harness.gateway.pool.set_model_slug(
            &AccountKey("OpenRouter".to_owned()),
            "glm-5.3-flash",
            "zai-org/glm-5.3-flash",
        );
        let ghost_url = harness.client_url.replacen("session-1", "ghost", 1);
        let response = post_model(&ghost_url, "glm-5.3-flash").await;
        assert_eq!(response.status(), StatusCode::OK);
        let recorded = harness.requests.lock().first().expect("the upstream saw a request").clone();
        // Parsed, not a substring: a re-introduced double-quote around
        // the value would satisfy a contains check and still be
        // invalid JSON (the exact bug this test guards).
        let body: serde_json::Value =
            serde_json::from_str(&recorded.body).expect("the forwarded body parses as JSON");
        assert_eq!(
            body.get("model").and_then(|m| m.as_str()),
            Some("zai-org/glm-5.3-flash"),
            "the forwarded body's model is exactly the upstream slug",
        );
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
        // a declared-model miss.
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

    /// **The reset a budget failure names is the one this org is waiting
    /// on.** The walk emptied because the caller's own accounts are
    /// cooling, so another org's account clearing sooner is not a time
    /// its retry can land - and a cron deferral prints this value as a
    /// wall clock, which is what turns a global minimum into a wrong
    /// time rather than only an imprecise one.
    #[tokio::test]
    async fn the_budget_failure_names_this_orgs_soonest_reset_not_another_orgs() {
        let harness = harness(Duration::ZERO).await;
        harness.gateway.set_org_pins([
            (
                "Busytools".to_owned(),
                crate::selection::OrgPin {
                    accounts: vec!["OpenRouter".to_owned()],
                    fallback_accounts: Vec::new(),
                },
            ),
            (
                "Other".to_owned(),
                crate::selection::OrgPin {
                    accounts: vec!["Anthropic".to_owned()],
                    fallback_accounts: Vec::new(),
                },
            ),
        ]);
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock after the epoch")
            .as_secs();
        harness
            .gateway
            .report_probe_limit(&AccountKey("OpenRouter".to_owned()), Some(now_secs + 600));
        harness
            .gateway
            .report_probe_limit(&AccountKey("Anthropic".to_owned()), Some(now_secs + 30));

        let Err(failure) = harness.gateway.select_for("Busytools", "glm-5.3-flash") else {
            panic!("the org's only account is cooling");
        };
        let SelectFailure::BudgetExhausted { tried, org, model, reset_in } = failure else {
            panic!("a cooling walk is budget exhaustion, got {failure:?}");
        };
        assert_eq!((tried, org.as_str(), model.as_str()), (1, "Busytools", "glm-5.3-flash"));
        assert!(
            reset_in >= Duration::from_secs(590) && reset_in <= Duration::from_secs(600),
            "the reset is this org's account, not Other's sooner one: {reset_in:?}",
        );
    }

    #[tokio::test]
    async fn a_declared_model_miss_stays_a_declared_model_miss_with_an_unrelated_cooldown_live() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "Anthropic");
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("Anthropic".to_owned()), crate::LoadingState::Ready);
        // An account OUTSIDE the walk is cooling; the walk itself holds
        // nothing cooling, so the walk emptied on the declared-model
        // gate and the failure must keep saying so.
        let reset = SystemTime::now() + Duration::from_secs(600);
        let reset_secs =
            reset.duration_since(SystemTime::UNIX_EPOCH).expect("future reset").as_secs();
        harness.gateway.report_probe_limit(&AccountKey("OpenRouter".to_owned()), Some(reset_secs));
        let ghost_url = harness.client_url.replacen("session-1", "ghost", 1);
        let response = post_model(&ghost_url, "glm-5.3-flash").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.expect("body");
        assert!(
            body.contains("no account in org 'Busytools' serves model 'glm-5.3-flash'"),
            "a declared-model miss keeps the declared-model message even with an unrelated cooldown live, got: {body}",
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
        harness.gateway.report_rate_limit("Busytools", "forge", "session-1", Some(0));
        let ghost_url = harness.client_url.replacen("session-1", "ghost", 1);
        let response = post_model(&ghost_url, "glm-5.3-flash").await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a zero reset reads as absent, so the default cooldown still applies"
        );
    }

    /// A session bound to an account the file removed keeps forwarding on
    /// it: retirement is not unbinding.
    #[tokio::test]
    async fn a_removed_account_still_serves_its_bound_session() {
        let harness = harness(Duration::ZERO).await;
        harness.gateway.pool.replace_accounts(&[]);

        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let requests = harness.requests.lock();
        assert_eq!(requests.len(), 1, "the removed account still forwards");
        assert_eq!(
            requests[0].first("authorization"),
            Some("Bearer real-openrouter-key"),
            "the retired account keeps the credential its session was bound with",
        );
    }

    /// A token re-minted for an account the file still holds reaches the
    /// session bound to it on its next request, so rotating a credential
    /// needs neither a restart nor a new session.
    #[tokio::test]
    async fn a_reminted_token_reaches_a_bound_session() {
        let harness = harness(Duration::ZERO).await;
        let rotated = HashMap::from([
            ("ANTHROPIC_BASE_URL".to_owned(), harness.upstream_url.clone()),
            ("ANTHROPIC_AUTH_TOKEN".to_owned(), "rotated-openrouter-key".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), String::new()),
        ]);
        harness.gateway.pool.replace_accounts(&[forge_primitives::account::LoadedAccount {
            display_name: "OpenRouter".to_owned(),
            provider: forge_primitives::account::Provider::Openrouter,
            base_url: None,
            models: vec!["glm-5.3-flash".to_owned()],
            model_aliases: std::collections::HashMap::new(),
            model_slugs: std::collections::HashMap::new(),
            env: rotated,
        }]);

        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let requests = harness.requests.lock();
        assert_eq!(requests.len(), 1, "exactly one upstream request");
        assert_eq!(
            requests[0].first("authorization"),
            Some("Bearer rotated-openrouter-key"),
            "the re-minted token rides the next request on the bound session",
        );
    }

    #[tokio::test]
    async fn a_model_change_keeps_the_bound_account() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "Anthropic");
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("Anthropic".to_owned()), crate::LoadingState::Ready);
        // session-1 is bound to OpenRouter; the body now asks for a
        // claude model. The binding stays: a running session keeps the
        // account it spawned under.
        let response = post_model(&harness.client_url, "claude-sonnet-5").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            Some(AccountKey("OpenRouter".to_owned())),
            "the model change leaves the binding where it was",
        );
        assert_eq!(harness.requests.lock().len(), 1, "the request was forwarded once");
    }

    #[tokio::test]
    async fn a_model_change_with_no_serving_account_still_uses_the_binding() {
        let harness = harness(Duration::ZERO).await;
        pin_only(&harness, "OpenRouter");
        harness
            .gateway
            .pool
            .set_loading(&AccountKey("OpenRouter".to_owned()), crate::LoadingState::Ready);
        // Bound to OpenRouter and asking for claude: the bound account is
        // still the session's account, so the request is forwarded rather
        // than failing on a selection walk nobody asked for.
        let response = post_model(&harness.client_url, "claude-sonnet-5").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            harness.gateway.bindings.binding_for("Busytools", "forge", "session-1"),
            Some(AccountKey("OpenRouter".to_owned())),
            "the binding is kept",
        );
        assert_eq!(harness.requests.lock().len(), 1, "the request is forwarded once");
    }

    #[tokio::test]
    async fn a_non_claude_model_on_an_anthropic_binding_sticks() {
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
            Some(AccountKey("Anthropic".to_owned())),
            "a bound session is not moved by the model in a request body",
        );
    }

    /// `select_for` answers the walk for an org and model WITHOUT
    /// binding: the spawn calls it and registers its own binding, and
    /// the launchpad reads it to show what a spawn would do.
    #[test]
    fn select_for_walks_without_binding() {
        let account = |name: &str| forge_primitives::account::LoadedAccount {
            display_name: name.to_owned(),
            provider: forge_primitives::account::Provider::Anthropic,
            base_url: None,
            models: vec!["claude-sonnet-5".to_owned()],
            model_aliases: std::collections::HashMap::new(),
            model_slugs: std::collections::HashMap::new(),
            env: std::collections::HashMap::new(),
        };
        let pool = Arc::new(crate::AccountPool::new(&[account("Cooling"), account("Ready")]));
        for name in ["Cooling", "Ready"] {
            pool.set_loading(&AccountKey(name.to_owned()), crate::LoadingState::Ready);
        }
        let gateway = Gateway::new(Arc::clone(&pool), reqwest::Client::new());
        gateway.set_org_pins([(
            "Default".to_owned(),
            crate::selection::OrgPin {
                accounts: vec!["Cooling".to_owned(), "Ready".to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
        // The first account in walk order is inside a rotation cooldown.
        gateway.rotation.lock().cool_down(
            &AccountKey("Cooling".to_owned()),
            SystemTime::now() + Duration::from_secs(60),
        );

        let selected = gateway
            .select_for("Default", "claude-sonnet-5")
            .expect("the ready account serves the model");
        assert_eq!(selected, AccountKey("Ready".to_owned()), "the walk skips a cooling account");
        assert!(
            pool.provider(&selected).is_some(),
            "the walk only ever returns an account the pool holds. A spawn's registration comes \
             from `provider` on this key, so this is what keeps a pooled session's registration \
             `Some` - and with it the bridge's id slot non-empty",
        );
        assert_eq!(
            gateway.bindings.binding_for("Default", "project", "session"),
            None,
            "selection writes no binding; the caller owns that",
        );
    }

    /// **The read surface's walk answers while the account is cooling.** A
    /// chip must name the account a spawn would land on once the window
    /// lifts, and the selecting walk keeps refusing exactly as it did.
    fn cooling_account(name: &str) -> forge_primitives::account::LoadedAccount {
        forge_primitives::account::LoadedAccount {
            display_name: name.to_owned(),
            provider: forge_primitives::account::Provider::Anthropic,
            base_url: None,
            models: vec!["claude-sonnet-5".to_owned()],
            model_aliases: std::collections::HashMap::new(),
            model_slugs: std::collections::HashMap::new(),
            env: std::collections::HashMap::new(),
        }
    }

    /// **The read names the account a spawn lands on first, not the
    /// first-pinned one.** With two cooling accounts the walk thaws the one
    /// whose reset is soonest, so pin order alone names the wrong account.
    #[test]
    fn cooling_account_serving_names_the_account_that_thaws_first() {
        let pool = Arc::new(crate::AccountPool::new(&[
            cooling_account("First"),
            cooling_account("Second"),
        ]));
        for name in ["First", "Second"] {
            pool.set_loading(&AccountKey(name.to_owned()), crate::LoadingState::Ready);
        }
        let gateway = Gateway::new(Arc::clone(&pool), reqwest::Client::new());
        gateway.set_org_pins([(
            "Default".to_owned(),
            crate::selection::OrgPin {
                accounts: vec!["First".to_owned(), "Second".to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
        let now = SystemTime::now();
        let first_until = now + Duration::from_secs(2 * 3600);
        let second_until = now + Duration::from_secs(10 * 60);
        gateway.rotation.lock().cool_down(&AccountKey("First".to_owned()), first_until);
        gateway.rotation.lock().cool_down(&AccountKey("Second".to_owned()), second_until);

        assert!(
            matches!(
                gateway.select_for("Default", "claude-sonnet-5"),
                Err(SelectFailure::BudgetExhausted { .. }),
            ),
            "selection keeps refusing a cooling walk",
        );
        assert_eq!(
            gateway.cooling_account_serving("Default", "claude-sonnet-5"),
            Some(AccountKey("Second".to_owned())),
            "the read names the account whose cooldown ends first",
        );
        assert_eq!(
            gateway.bindings.binding_for("Default", "project", "session"),
            None,
            "the read writes no binding",
        );

        // The claim itself: thaw the second account and the walk takes it.
        gateway
            .rotation
            .lock()
            .cool_down(&AccountKey("Second".to_owned()), now - Duration::from_secs(1));
        assert_eq!(
            gateway
                .select_for("Default", "claude-sonnet-5")
                .expect("the thawed account is selectable"),
            AccountKey("Second".to_owned()),
            "the first spawn after the read lands on the account it named",
        );
    }

    /// One cooling account declaring the model is the read's answer, and a
    /// cooling account the model does not name is not.
    #[test]
    fn cooling_account_serving_respects_the_declared_model() {
        let mut elsewhere = cooling_account("Elsewhere");
        elsewhere.models = vec!["claude-opus-5".to_owned()];
        let pool = Arc::new(crate::AccountPool::new(&[cooling_account("Serving"), elsewhere]));
        for name in ["Serving", "Elsewhere"] {
            pool.set_loading(&AccountKey(name.to_owned()), crate::LoadingState::Ready);
        }
        let gateway = Gateway::new(Arc::clone(&pool), reqwest::Client::new());
        gateway.set_org_pins([(
            "Default".to_owned(),
            crate::selection::OrgPin {
                accounts: vec!["Elsewhere".to_owned(), "Serving".to_owned()],
                fallback_accounts: Vec::new(),
            },
        )]);
        let now = SystemTime::now();
        gateway
            .rotation
            .lock()
            .cool_down(&AccountKey("Serving".to_owned()), now + Duration::from_secs(3600));

        assert_eq!(
            gateway.cooling_account_serving("Default", "claude-sonnet-5"),
            Some(AccountKey("Serving".to_owned())),
            "only the account declaring the model answers the read",
        );
    }

    /// The published walk order is readable, so a view can render the
    /// orgs the gateway actually holds. Sorted by org name for a stable
    /// render: the store underneath is a HashMap.
    #[test]
    fn published_org_pins_come_back_sorted_with_their_lists_intact() {
        let gateway =
            Gateway::new(Arc::new(crate::AccountPool::empty_for_test()), reqwest::Client::new());
        let pin = |accounts: &[&str], fallbacks: &[&str]| crate::selection::OrgPin {
            accounts: accounts.iter().map(|name| (*name).to_owned()).collect(),
            fallback_accounts: fallbacks.iter().map(|name| (*name).to_owned()).collect(),
        };
        gateway.set_org_pins([
            ("Zed".to_owned(), pin(&["B"], &["C"])),
            ("Alpha".to_owned(), pin(&["A"], &[])),
            ("Mike".to_owned(), pin(&["M"], &[])),
            ("Bravo".to_owned(), pin(&["N"], &["O", "P"])),
        ]);

        let pins = gateway.org_pins();
        let names: Vec<&str> = pins.iter().map(|(org, _)| org.as_str()).collect();
        assert_eq!(
            names,
            vec!["Alpha", "Bravo", "Mike", "Zed"],
            "every published org comes back in name order",
        );
        assert_eq!(pins[1].1.accounts, vec!["N".to_owned()], "a primary list survives");
        assert_eq!(
            pins[1].1.fallback_accounts,
            vec!["O".to_owned(), "P".to_owned()],
            "a fallback list survives in order",
        );
    }

    /// **A cooldown write is published, with the end a reader sleeps to.** A
    /// project row's bindable answer changes on the cool and again on the
    /// clock-driven thaw; the gateway owns both moments and the reader
    /// outside this crate can only learn the first from a write.
    #[tokio::test]
    async fn a_cooldown_write_is_published_with_its_end() {
        let pool = Arc::new(crate::AccountPool::empty_for_test());
        let gateway = Gateway::new(pool, reqwest::Client::new());
        let watch = gateway.rotation_watch();
        let key = AccountKey("Cooling".to_owned());
        let now = SystemTime::now();
        assert_eq!(gateway.soonest_cooldown_end(now), None, "nothing is cooling yet");
        assert!(!watch.has_changed().expect("this test holds the watch"), "nothing published yet");

        let reset_at =
            now.duration_since(std::time::UNIX_EPOCH).expect("after the epoch").as_secs() + 60;
        gateway.report_probe_limit(&key, Some(reset_at));

        assert!(
            watch.has_changed().expect("this test holds the watch"),
            "the cooldown write is published, or the roster never learns of it",
        );
        assert_eq!(
            gateway.soonest_cooldown_end(SystemTime::now()),
            Some(crate::rotation::reset_instant(reset_at)),
            "the published end is the one the reader sleeps to",
        );
    }

    /// A synthetic error with a hand-picked cause, so the chain walker and
    /// the classifier are exercised on shapes reqwest's own errors cannot
    /// be built into.
    #[derive(Debug)]
    struct Chained {
        message: &'static str,
        source: Option<Box<Chained>>,
    }

    impl std::fmt::Display for Chained {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.message)
        }
    }

    impl std::error::Error for Chained {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_ref().map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
        }
    }

    fn chain(messages: &[&'static str]) -> Chained {
        let mut messages = messages.to_vec();
        let mut error =
            Chained { message: messages.pop().expect("a chain has a root layer"), source: None };
        while let Some(message) = messages.pop() {
            error = Chained { message, source: Some(Box::new(error)) };
        }
        error
    }

    /// A `source()` that returns itself, the shape the depth cap exists for.
    #[derive(Debug)]
    struct Loops;

    impl std::fmt::Display for Loops {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("loops forever")
        }
    }

    impl std::error::Error for Loops {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self)
        }
    }

    /// reqwest's own `Display` is the top layer only; the cause a reader
    /// needs (DNS, connect, TLS, reset, EMFILE) sits below it, so the whole
    /// walk has to cross in one line.
    #[test]
    fn the_chain_describer_names_every_cause() {
        let flat = Chained { message: "request failed", source: None };
        assert_eq!(
            describe_error_chain(&flat),
            "request failed",
            "a flat error is its own whole chain",
        );

        let nested = chain(&[
            "error sending request for url (https://openrouter.ai/api/v1/messages)",
            "client error (Connect)",
            "tcp connect error: Connection refused",
        ]);
        assert_eq!(
            describe_error_chain(&nested),
            "error sending request for url (https://openrouter.ai/api/v1/messages) -> client \
             error (Connect) -> tcp connect error: Connection refused",
            "every layer crosses, top first and root last",
        );
    }

    /// A wrapper whose Display delegates to its cause renders the same text
    /// twice; the walk has to pass it and still reach the root.
    #[test]
    fn the_chain_describer_passes_a_repeated_layer() {
        assert_eq!(
            describe_error_chain(&chain(&["same text", "same text", "the root"])),
            "same text -> the root",
            "the repeat is skipped, the root below it is not lost",
        );
    }

    /// A capped chain says so, so its last line cannot read as the root -
    /// and a source that returns itself terminates.
    #[test]
    fn the_chain_walk_is_bounded_and_says_so() {
        let messages: [&str; 20] = [
            "layer 0", "layer 1", "layer 2", "layer 3", "layer 4", "layer 5", "layer 6", "layer 7",
            "layer 8", "layer 9", "layer 10", "layer 11", "layer 12", "layer 13", "layer 14",
            "layer 15", "layer 16", "layer 17", "layer 18", "layer 19",
        ];
        let described = describe_error_chain(&chain(&messages));
        assert!(
            described.ends_with("... (truncated at 16 layers)"),
            "a capped chain carries the marker: {described}",
        );
        assert_eq!(
            described.split(" -> ").count(),
            MAX_ERROR_CHAIN_DEPTH + 1,
            "the walk keeps the cap and adds the marker: {described}",
        );

        let looped = describe_error_chain(&Loops);
        assert!(
            looped.ends_with("... (truncated at 16 layers)"),
            "a self-referential source terminates at the cap: {looped}",
        );
    }

    /// The kind has to come off the chain, not the top, and every real
    /// reqwest connect-phase error carries a `client error (Connect)`
    /// wrapper above its cause - so each connect-phase case here holds a
    /// "connect" token above its own kind, pinning the precedence and not
    /// just the needles. The send-phase cases below it open with `client
    /// error (SendRequest)` instead.
    #[test]
    fn the_classifier_reads_the_kind_off_the_chain() {
        let cases = [
            (
                chain(&[
                    "error sending request for url (https://openrouter.ai/api/v1/messages)",
                    "client error (Connect)",
                    "dns error: failed to lookup address information: nodename nor servname \
                     provided, or not known",
                ]),
                ForwardFailureKind::Dns,
            ),
            (
                chain(&[
                    "error sending request",
                    "client error (Connect)",
                    "tcp connect error: Connection refused",
                ]),
                ForwardFailureKind::Connect,
            ),
            (
                chain(&["error sending request", "client error (Connect)", "tls handshake eof"]),
                ForwardFailureKind::Tls,
            ),
            (
                chain(&[
                    "error sending request",
                    "client error (Connect)",
                    "connection reset by peer",
                ]),
                ForwardFailureKind::Reset,
            ),
            (
                chain(&[
                    "error sending request",
                    "client error (Connect)",
                    "connection closed before message completed",
                ]),
                ForwardFailureKind::Closed,
            ),
            (
                chain(&["error sending request", "client error (Connect)", "operation timed out"]),
                ForwardFailureKind::Timeout,
            ),
            (
                chain(&[
                    "error sending request",
                    "client error (Connect)",
                    "tcp connect error: Too many open files (os error 24)",
                ]),
                ForwardFailureKind::Fd,
            ),
            (
                // The only matching token is upper case: the classifier has
                // to fold before it compares.
                chain(&[
                    "error sending request",
                    "client error (Connect)",
                    "TLS handshake failure",
                ]),
                ForwardFailureKind::Tls,
            ),
            (
                // reqwest's top Display embeds the request URL; a host
                // carrying a kind word there must not decide the kind.
                chain(&[
                    "error sending request for url (https://ssl-proxy.internal/v1/messages)",
                    "tcp connect error: Connection refused",
                ]),
                ForwardFailureKind::Connect,
            ),
            (
                // The measured shape behind every gateway_forward_failed
                // record of 2026-11-14: a peer RST_STREAM.
                chain(&[
                    "error sending request for url \
                     (https://openrouter.ai/api/v1/messages?beta=true)",
                    "client error (SendRequest)",
                    "http2 error",
                    "stream error received: unspecific protocol error detected",
                ]),
                ForwardFailureKind::Http2,
            ),
            (
                // A peer that closes the stream stays "closed". hyper sends
                // the h2 cause as its own source layer, so this pins both
                // the precedence and the search below the wrapper.
                chain(&[
                    "error sending request for url (https://openrouter.ai/api/v1/messages)",
                    "client error (SendRequest)",
                    "http2 error",
                    "connection closed because of a broken pipe",
                ]),
                ForwardFailureKind::Closed,
            ),
            (
                // The wrapper is not required for the h2 kind: the cause
                // layer's own words are enough.
                chain(&[
                    "error sending request for url (https://openrouter.ai/api/v1/messages)",
                    "client error (SendRequest)",
                    "stream error received: unspecific protocol error detected",
                ]),
                ForwardFailureKind::Http2,
            ),
            (
                // REFUSED_STREAM's cause layer carries "refused", which is
                // connect by precedence; the h2 kind sitting last is what
                // keeps that answer.
                chain(&[
                    "error sending request for url (https://openrouter.ai/api/v1/messages)",
                    "client error (SendRequest)",
                    "http2 error",
                    "refused stream before processing any application logic",
                ]),
                ForwardFailureKind::Connect,
            ),
            (
                // A flat error has no wrapper to skip, so its own text is
                // still evidence.
                Chained { message: "tcp connect error: Connection refused", source: None },
                ForwardFailureKind::Connect,
            ),
            (
                Chained { message: "something else entirely", source: None },
                ForwardFailureKind::Other,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(
                classify_forward_failure(&error),
                expected,
                "chain '{}' classifies as {expected:?}",
                describe_error_chain(&error),
            );
        }
    }

    /// A needle below the cap is never reached, so the classifier is
    /// bounded by the same walk the describer uses.
    #[test]
    fn the_classifier_does_not_read_past_the_cap() {
        let mut messages: [&str; 20] = [
            "layer 0", "layer 1", "layer 2", "layer 3", "layer 4", "layer 5", "layer 6", "layer 7",
            "layer 8", "layer 9", "layer 10", "layer 11", "layer 12", "layer 13", "layer 14",
            "layer 15", "layer 16", "layer 17", "layer 18", "layer 19",
        ];
        messages[17] = "connection reset by peer";
        assert_eq!(
            classify_forward_failure(&chain(&messages)),
            ForwardFailureKind::Other,
            "the needle past the cap is not read",
        );
    }

    #[test]
    fn every_kind_names_itself() {
        for (kind, name) in [
            (ForwardFailureKind::Dns, "dns"),
            (ForwardFailureKind::Connect, "connect"),
            (ForwardFailureKind::Tls, "tls"),
            (ForwardFailureKind::Reset, "reset"),
            (ForwardFailureKind::Closed, "closed"),
            (ForwardFailureKind::Timeout, "timeout"),
            (ForwardFailureKind::Fd, "fd"),
            (ForwardFailureKind::Http2, "http2"),
            (ForwardFailureKind::Other, "other"),
        ] {
            assert_eq!(kind.as_str(), name);
        }
    }

    /// One captured record: level, target, and `name=value` fields.
    type CaughtRecord = (tracing::Level, &'static str, String);

    /// Every record a site emitted while the capture was installed.
    #[derive(Clone, Default)]
    struct Caught(Arc<std::sync::Mutex<Vec<CaughtRecord>>>);

    #[derive(Default)]
    struct Fields(String);

    impl tracing::field::Visit for Fields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            let _ = std::fmt::write(&mut self.0, format_args!("{}={value:?}", field.name()));
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Caught {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0.lock().expect("capture").push((
                *event.metadata().level(),
                event.metadata().target(),
                fields.0,
            ));
        }
    }

    /// The line this change exists for: the 502 body carries no cause, so
    /// the record is the only place it survives. Deleting the warn block,
    /// demoting its level or dropping a field must fail here.
    #[tokio::test]
    async fn a_failed_forward_is_recorded_with_its_cause_chain() {
        use tracing_subscriber::layer::SubscriberExt;

        let harness = harness(Duration::ZERO).await;
        // A port the probe bound and dropped: the forward's connect is
        // refused for real, through reqwest's own error chain.
        let dead_port = free_port().await;
        let dead_upstream = format!("http://127.0.0.1:{dead_port}");
        let dead_env = HashMap::from([
            ("ANTHROPIC_BASE_URL".to_owned(), dead_upstream.clone()),
            ("ANTHROPIC_AUTH_TOKEN".to_owned(), "real-openrouter-key".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), String::new()),
        ]);
        harness.gateway.pool.replace_accounts(&[forge_primitives::account::LoadedAccount {
            display_name: "OpenRouter".to_owned(),
            provider: forge_primitives::account::Provider::Openrouter,
            base_url: None,
            models: vec!["glm-5.3-flash".to_owned()],
            model_aliases: std::collections::HashMap::new(),
            model_slugs: std::collections::HashMap::new(),
            env: dead_env,
        }]);

        let caught = Caught::default();
        let subscriber = tracing_subscriber::Registry::default().with(caught.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let response = post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response.text().await.expect("a 502 body");
        assert!(body.starts_with("upstream request failed: "), "the 502 body is unchanged: {body}");

        let caught = caught.0.lock().expect("capture").clone();
        let ours: Vec<_> =
            caught.iter().filter(|(_, target, _)| *target == "forge_gateway::forward").collect();
        assert_eq!(ours.len(), 1, "one failed forward emits one record: {caught:?}");
        let (level, _, fields) = &ours[0];
        assert_eq!(*level, tracing::Level::WARN, "the line claims forge's own problem: {fields}");
        for needle in [
            "event_name=\"gateway_forward_failed\"",
            "account=OpenRouter",
            "org=Busytools",
            "project=forge",
            "session=session-1",
            "model=glm-5.3-flash",
            &format!("upstream={dead_upstream}/v1/messages?beta=true"),
            "kind=\"connect\"",
            "elapsed_ms=",
            "error_chain=",
            " -> ",
            "Connection refused",
        ] {
            assert!(fields.contains(needle), "{needle} missing from {fields}");
        }
        assert!(
            !fields.contains("real-openrouter-key"),
            "the record must never carry the credential: {fields}",
        );
    }

    /// The other silent arm: the upstream answered and its head is already
    /// on the wire, so the body dying mid-stream is a cut the CLI retries
    /// with no cause. Deleting the stream's warn block, demoting its level
    /// or dropping a field must fail here.
    #[tokio::test]
    async fn a_stream_cut_after_the_headers_is_recorded_with_its_cause_chain() {
        use tracing_subscriber::layer::SubscriberExt;

        let harness = harness_with_tail(Duration::from_millis(50), StubBody::Cut).await;
        let caught = Caught::default();
        let subscriber = tracing_subscriber::Registry::default().with(caught.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut response =
            post_as_cli(&format!("{}/v1/messages?beta=true", harness.client_url)).await;
        assert_eq!(response.status(), StatusCode::OK, "the head reached the CLI before the cut");
        let first = response.chunk().await.expect("a first chunk").expect("chunk bytes");
        assert_eq!(&first[..], b"event: first\n", "the live prefix reached the CLI");
        let cut = response.chunk().await;
        assert!(cut.is_err(), "the body is cut mid-stream, not completed; got {cut:?}");

        let caught = caught.0.lock().expect("capture").clone();
        let ours: Vec<_> =
            caught.iter().filter(|(_, target, _)| *target == "forge_gateway::forward").collect();
        assert_eq!(ours.len(), 1, "one cut stream emits one record: {caught:?}");
        let (level, _, fields) = &ours[0];
        assert_eq!(*level, tracing::Level::WARN, "the line claims forge's own problem: {fields}");
        for needle in [
            "event_name=\"gateway_forward_stream_failed\"",
            "account=OpenRouter",
            "org=Busytools",
            "project=forge",
            "session=session-1",
            "model=glm-5.3-flash",
            &format!("upstream={}/v1/messages?beta=true", harness.upstream_url),
            "request_id=req-test",
            "status=200",
            "kind=\"closed\"",
            "stream_elapsed_ms=",
            "error_chain=",
            " -> ",
            "unexpected EOF during chunk size line",
        ] {
            assert!(fields.contains(needle), "{needle} missing from {fields}");
        }
        // The clock is the stream's own: the stub holds the body for 50 ms
        // after its first chunk, so a clock taken at the error site reads 0.
        let elapsed = fields
            .split("stream_elapsed_ms=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u128>().ok())
            .expect("the record carries the stream's elapsed ms");
        assert!(elapsed >= 50, "the stream ran for its own 50 ms at least; got {elapsed}");
        assert!(
            !fields.contains("real-openrouter-key"),
            "the record must never carry the credential: {fields}",
        );
    }
}
