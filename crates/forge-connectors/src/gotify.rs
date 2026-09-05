//! The Gotify connector: WebSocket receive stream, `/application` +
//! `/message` REST lookups, the subscription matcher, and the
//! reconnecting subsystem pump that routes matched messages into the
//! host's sessions.
//!
//! Everything the connector needs from the workspace arrives through
//! the [`GotifyHost`] port, so this module stays stream + mapping and
//! is testable offline.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Context;
use forge_primitives::{GotifyConfig, GotifyMessage, GotifySubscription};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// The host port, implemented by forge-workspace. The only
/// workspace-side state, delivery or TLS-trust plumbing a connector
/// may reach, so this crate stays stream + mapping.
pub trait GotifyHost: Send + Sync {
    /// A reqwest client with the NODE_EXTRA_CA_CERTS roots applied
    /// and the caller's timeout baked in.
    fn http_client(&self, timeout: Duration) -> Result<reqwest::Client, String>;

    /// The active subscriptions to match inbound messages against.
    fn subscriptions(&self) -> Vec<GotifySubscription>;

    /// The application NAME for `appid` via the app index, or `None`
    /// when the id isn't known (index not yet fetched, or a new app
    /// the server added after the last refresh).
    fn app_name(&self, appid: u64) -> Option<String>;

    /// Replace the fetched name->appid index.
    fn store_app_index(&self, index: HashMap<String, u64>);

    /// Record stream liveness for the Inspector's GOTIFY status line.
    fn set_connected(&self, connected: bool);

    /// Deliver one matched message to its subscriber's session:
    /// dispatch a user-turn into the running target, or buffer + spawn
    /// the asleep one.
    fn deliver(&self, subscription: &GotifySubscription, app: &str, message: &GotifyMessage);
}

/// Reconnect backoff: starts at 500ms, doubles per failed/short-lived
/// connect, caps at 30s, and resets to the floor only after a connection
/// proves healthy (see [`MIN_HEALTHY_UPTIME`]).
const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Minimum time a connection must stay up to count as healthy. A session
/// that drops sooner keeps the backoff escalating, so an accept-then-drop
/// server (post-upgrade auth reject, an idle-conn-dropping proxy) can't
/// spin at the 500ms floor.
const MIN_HEALTHY_UPTIME: Duration = Duration::from_secs(5);

/// Dial timeout so a stuck `connect_async` (no OS-level deadline) can't
/// hang the loop; the dial is also raced against shutdown so a signal is
/// observed promptly rather than bounded by the OS TCP timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Send a WebSocket keepalive ping this often on an established stream, so a
/// half-open path surfaces as an unanswered ping (write error or no returning
/// pong) instead of a silently blocked read that never wakes.
const PING_INTERVAL: Duration = Duration::from_secs(22);

/// Treat an established stream's path as dead when no frame (message, server
/// ping, or pong) arrives within this window. Sits comfortably past both 2x
/// [`PING_INTERVAL`] and Gotify's own ~45s server-ping period, so a healthy
/// but quiet stream never trips it; a half-open socket left by a silent
/// network drop does, which breaks the read loop into a reconnect.
const IDLE_DEADLINE: Duration = Duration::from_secs(80);

/// Per-request timeout for the read-only REST lookups (`/application`,
/// `/message`) - keep a slow or unreachable server from stalling
/// subsystem start or a tool call.
const REST_TIMEOUT: Duration = Duration::from_secs(10);

/// Newest `/message` window pulled before client-side app + priority
/// filtering. Gotify's `/message` filters by neither, so we fetch the
/// newest page and narrow locally; 200 is its per-request maximum.
const RECENT_FETCH_WINDOW: u32 = 200;

/// Connection-lifecycle + message events from the stream task to the
/// subsystem pump.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GotifyEvent {
    Connected,
    Disconnected,
    Message(GotifyMessage),
}

/// One entry of Gotify's `GET /application` response. Fields beyond
/// `id` + `name` (token, description, image) are ignored.
#[derive(Debug, Deserialize)]
struct GotifyApp {
    id: u64,
    name: String,
}

/// Gotify's `GET /message` paged response. Only the newest page of
/// `messages` is consumed; the `paging` cursor is ignored - a catch-up
/// read wants the newest N, not the full history.
#[derive(Debug, Deserialize)]
struct GotifyMessages {
    messages: Vec<GotifyMessage>,
}

/// One catch-up notification from `GET /message`, resolved for a
/// `gotify__recent` reply: the application display name (from the appid,
/// or the id as a string when the app index hasn't seen it), title,
/// message body, priority, and the server's verbatim RFC3339 timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GotifyRecent {
    pub app: String,
    pub title: String,
    pub message: String,
    pub priority: u8,
    pub date: String,
}

/// A reqwest client for the read-only REST lookups, carrying the shared
/// native-roots trust extension every forge HTTPS site uses.
fn rest_client(host: &dyn GotifyHost) -> anyhow::Result<reqwest::Client> {
    host.http_client(REST_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("build Gotify http client: {error}"))
}

/// GET the server's application list. Shared by the name->id index, the
/// `gotify__apps` name list, and `gotify__recent`'s appid->name resolution.
async fn fetch_apps(host: &dyn GotifyHost, cfg: &GotifyConfig) -> anyhow::Result<Vec<GotifyApp>> {
    let url = format!("{}/application", cfg.url.trim_end_matches('/'));
    rest_client(host)?
        .get(url)
        .header("X-Gotify-Key", &cfg.client_token)
        .send()
        .await
        .context("GET /application")?
        .error_for_status()
        .context("/application returned an error status")?
        .json()
        .await
        .context("parse /application body")
}

/// Fetch the server's application list and fold it into a name->appid
/// map. Inbound stream messages carry the numeric appid, so the map
/// resolves an `application` name filter to the id to match against.
pub async fn app_index(
    host: &dyn GotifyHost,
    cfg: &GotifyConfig,
) -> anyhow::Result<HashMap<String, u64>> {
    Ok(build_app_index(fetch_apps(host, cfg).await?))
}

fn build_app_index(apps: Vec<GotifyApp>) -> HashMap<String, u64> {
    apps.into_iter().map(|app| (app.name, app.id)).collect()
}

/// The server's application NAMEs (from `GET /application`), in the order
/// the server returns them. Backs `gotify__apps` so a session can
/// self-discover which apps it may subscribe to.
pub async fn app_names(host: &dyn GotifyHost, cfg: &GotifyConfig) -> anyhow::Result<Vec<String>> {
    Ok(build_app_names(fetch_apps(host, cfg).await?))
}

fn build_app_names(apps: Vec<GotifyApp>) -> Vec<String> {
    apps.into_iter().map(|app| app.name).collect()
}

fn build_id_index(apps: Vec<GotifyApp>) -> HashMap<u64, String> {
    apps.into_iter().map(|app| (app.id, app.name)).collect()
}

/// The most recent notifications, newest first, filtered by application
/// NAME (empty = any) and `min_priority` (`None` = any), capped at `limit`.
/// Fetches the newest `/message` window plus `/application` (for appid->
/// name), then narrows locally - Gotify's `/message` filters by neither.
pub async fn recent_messages(
    host: &dyn GotifyHost,
    cfg: &GotifyConfig,
    applications: &[String],
    min_priority: Option<u8>,
    limit: usize,
) -> anyhow::Result<Vec<GotifyRecent>> {
    let messages = fetch_messages(host, cfg, RECENT_FETCH_WINDOW).await?;
    let id_index = build_id_index(fetch_apps(host, cfg).await?);
    Ok(filter_recent(messages, &id_index, applications, min_priority, limit))
}

async fn fetch_messages(
    host: &dyn GotifyHost,
    cfg: &GotifyConfig,
    limit: u32,
) -> anyhow::Result<Vec<GotifyMessage>> {
    let url = format!("{}/message?limit={limit}", cfg.url.trim_end_matches('/'));
    let page: GotifyMessages = rest_client(host)?
        .get(url)
        .header("X-Gotify-Key", &cfg.client_token)
        .send()
        .await
        .context("GET /message")?
        .error_for_status()
        .context("/message returned an error status")?
        .json()
        .await
        .context("parse /message body")?;
    Ok(page.messages)
}

/// Resolve appid->name, filter by app + priority, sort newest-first (by
/// monotonic id), and truncate to `limit`. App matching mirrors `route`:
/// an unresolved appid never matches a
/// non-empty name filter (its numeric display string can't sneak past),
/// though its display `app` still falls back to that id string.
fn filter_recent(
    mut messages: Vec<GotifyMessage>,
    id_index: &HashMap<u64, String>,
    applications: &[String],
    min_priority: Option<u8>,
    limit: usize,
) -> Vec<GotifyRecent> {
    messages.sort_unstable_by_key(|m| std::cmp::Reverse(m.id));
    messages
        .into_iter()
        .filter_map(|msg| {
            let resolved = id_index.get(&msg.appid);
            let priority_ok = min_priority.is_none_or(|floor| msg.priority >= floor);
            let app_ok =
                applications.is_empty() || resolved.is_some_and(|name| applications.contains(name));
            if !(priority_ok && app_ok) {
                return None;
            }
            Some(GotifyRecent {
                app: resolved.cloned().unwrap_or_else(|| msg.appid.to_string()),
                title: msg.title,
                message: msg.message,
                priority: msg.priority,
                date: msg.date,
            })
        })
        .take(limit)
        .collect()
}

/// Parse a raw stream Text frame into a typed [`GotifyMessage`].
fn normalize(text: &str) -> anyhow::Result<GotifyMessage> {
    serde_json::from_str(text).context("parse Gotify stream message")
}

/// The subscriptions whose `min_priority` and `applications` filters
/// `priority` and `resolved_app` satisfy, in set order. `resolved_app`
/// is the message's application NAME resolved from the app index; an
/// unresolved appid matches only an empty (match-any) filter set, never
/// a filter that happens to list the id numerically.
pub fn matching_subscriptions<'a>(
    subs: &'a [GotifySubscription],
    resolved_app: Option<&str>,
    priority: u8,
) -> Vec<&'a GotifySubscription> {
    subs.iter()
        .filter(|sub| {
            let priority_ok = sub.min_priority.is_none_or(|floor| priority >= floor);
            let app_ok = sub.applications.is_empty()
                || resolved_app.is_some_and(|name| sub.applications.iter().any(|app| app == name));
            priority_ok && app_ok
        })
        .collect()
}

/// An open Gotify receive stream.
struct GotifyStream {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

/// How a pumped stream ended, steering the reconnect loop.
enum PumpOutcome {
    /// Shutdown fired or the event receiver went away - stop the run loop.
    Stop,
    /// The stream ended, errored, or went idle past the deadline - fall
    /// through to the reconnect backoff.
    Reconnect,
}

impl GotifyStream {
    /// Open `{ws,wss}://<host>/stream?token=<client_token>` - `wss` for
    /// an `https` url, `ws` for `http`.
    async fn connect(cfg: &GotifyConfig) -> anyhow::Result<Self> {
        let url = stream_url(cfg)?;
        let (ws, _resp) = connect_async(url.as_str()).await.context("connect Gotify stream")?;
        Ok(Self { ws })
    }

    /// Forward frames to `tx` until shutdown, a read error/close, or a dead
    /// path (no frame within [`IDLE_DEADLINE`]). Splits the socket so a
    /// keepalive ping and the read run concurrently: any inbound frame counts
    /// as liveness, so a healthy-but-quiet stream stays up while a half-open
    /// one is detected and dropped for the reconnect loop to redial. Text
    /// frames forward as messages; an unparseable one is skipped with a warn.
    async fn pump(
        self,
        tx: &mpsc::Sender<GotifyEvent>,
        shutdown: &mut oneshot::Receiver<()>,
    ) -> PumpOutcome {
        let (mut sink, mut frames) = self.ws.split();
        let mut ping = tokio::time::interval(PING_INTERVAL);
        // The first tick fires immediately; drop it so pings pace from now.
        ping.tick().await;
        let mut last_activity = Instant::now();
        loop {
            tokio::select! {
                _ = &mut *shutdown => return PumpOutcome::Stop,
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Bytes::new())).await.is_err() {
                        return PumpOutcome::Reconnect;
                    }
                    if idle_past_deadline(last_activity, Instant::now()) {
                        tracing::warn!(
                            target: "forge_connectors::gotify",
                            idle_secs = IDLE_DEADLINE.as_secs(),
                            "Gotify stream idle past deadline; treating path as dead",
                        );
                        return PumpOutcome::Reconnect;
                    }
                }
                frame = frames.next() => match frame {
                    Some(Ok(Message::Text(text))) => {
                        last_activity = Instant::now();
                        match normalize(text.as_str()) {
                            Ok(msg) => {
                                if tx.send(GotifyEvent::Message(msg)).await.is_err() {
                                    return PumpOutcome::Stop;
                                }
                            }
                            Err(error) => {
                                tracing::warn!(target: "forge_connectors::gotify", %error, "skipping unparseable stream frame");
                            }
                        }
                    }
                    Some(Ok(_)) => last_activity = Instant::now(),
                    Some(Err(error)) => {
                        tracing::warn!(target: "forge_connectors::gotify", %error, "Gotify stream read error");
                        return PumpOutcome::Reconnect;
                    }
                    None => return PumpOutcome::Reconnect,
                }
            }
        }
    }
}

fn stream_url(cfg: &GotifyConfig) -> anyhow::Result<String> {
    let base = cfg.url.trim_end_matches('/');
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        anyhow::bail!("Gotify url must start with http:// or https://: {}", cfg.url);
    };
    Ok(format!("{ws_base}/stream?token={}", cfg.client_token))
}

/// Next reconnect delay after a session ended: reset to the floor when the
/// connection proved healthy (stayed up past [`MIN_HEALTHY_UPTIME`]), else
/// escalate (double, capped at [`BACKOFF_CAP`]).
fn next_backoff(current: Duration, healthy: bool) -> Duration {
    if healthy { BACKOFF_START } else { (current * 2).min(BACKOFF_CAP) }
}

/// Whether the stream has gone silent past [`IDLE_DEADLINE`] as of `now` -
/// no inbound frame refreshed `last_activity` in time, so the path is treated
/// as dead. Any frame resets `last_activity`, which resets this.
fn idle_past_deadline(last_activity: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_activity) > IDLE_DEADLINE
}

/// Long-lived reconnect loop: connect (emit [`GotifyEvent::Connected`]),
/// forward each message as [`GotifyEvent::Message`], and on drop/error
/// emit [`GotifyEvent::Disconnected`] then retry with exponential backoff.
/// The backoff resets to the floor only after a healthy session (one that
/// stayed up past `MIN_HEALTHY_UPTIME`); a fast drop or failed dial keeps
/// it escalating. Exits when `shutdown` fires or the sender is dropped.
async fn run(
    cfg: GotifyConfig,
    tx: mpsc::Sender<GotifyEvent>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut backoff = BACKOFF_START;
    loop {
        // Race the dial against shutdown + a timeout so a stuck connect
        // can neither outlive a shutdown signal nor hang the loop.
        let dial = tokio::select! {
            _ = &mut shutdown => return,
            result = tokio::time::timeout(CONNECT_TIMEOUT, GotifyStream::connect(&cfg)) => result,
        };
        let healthy = match dial {
            Ok(Ok(stream)) => {
                let connected_at = tokio::time::Instant::now();
                if tx.send(GotifyEvent::Connected).await.is_err() {
                    return;
                }
                if let PumpOutcome::Stop = stream.pump(&tx, &mut shutdown).await {
                    return;
                }
                let _ = tx.send(GotifyEvent::Disconnected).await;
                connected_at.elapsed() >= MIN_HEALTHY_UPTIME
            }
            Ok(Err(error)) => {
                tracing::warn!(target: "forge_connectors::gotify", %error, "Gotify connect failed; backing off");
                let _ = tx.send(GotifyEvent::Disconnected).await;
                false
            }
            Err(_) => {
                tracing::warn!(
                    target: "forge_connectors::gotify",
                    timeout_secs = CONNECT_TIMEOUT.as_secs(),
                    "Gotify connect timed out; backing off",
                );
                let _ = tx.send(GotifyEvent::Disconnected).await;
                false
            }
        };
        backoff = next_backoff(backoff, healthy);
        tokio::select! {
            _ = &mut shutdown => return,
            () = tokio::time::sleep(backoff) => {}
        }
    }
}

/// The Gotify subsystem pump: run the reconnecting stream task and
/// translate its events into host state + message routing. Holds the
/// host for its lifetime - the host impl is the weak boundary, so a
/// workspace-backed host degrades to no-ops once the workspace drops;
/// exits when `shutdown` fires or the stream task ends, dropping its
/// own handle to the stream task so that exits too.
pub async fn run_subsystem(
    host: std::sync::Arc<dyn GotifyHost>,
    cfg: GotifyConfig,
    mut shutdown: oneshot::Receiver<()>,
) {
    let (tx, mut rx) = mpsc::channel::<GotifyEvent>(64);
    // Dropping this handle at fn-end signals the stream task to exit.
    let (_run_shutdown_tx, run_shutdown_rx) = oneshot::channel();
    tokio::spawn(run(cfg.clone(), tx, run_shutdown_rx));

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            event = rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    GotifyEvent::Connected => {
                        // Refresh the app index off the pump so a slow (up to
                        // the 10s lookup timeout) or failing /application can't
                        // block shutdown or message routing.
                        tokio::spawn(refresh_app_index(host.clone(), cfg.clone()));
                        host.set_connected(true);
                    }
                    GotifyEvent::Disconnected => {
                        host.set_connected(false);
                    }
                    GotifyEvent::Message(msg) => {
                        route(&*host, &msg);
                    }
                }
            }
        }
    }

    host.set_connected(false);
}

/// Fetch the Gotify `/application` list and store the name->appid map.
/// Warns (never silently drops) on failure - an unresolved index would
/// otherwise leave every application-name-filtered subscription silently
/// matching nothing while the stream still reports connected.
async fn refresh_app_index(host: std::sync::Arc<dyn GotifyHost>, cfg: GotifyConfig) {
    match app_index(&*host, &cfg).await {
        Ok(index) => host.store_app_index(index),
        Err(error) => {
            tracing::warn!(
                target: "forge_connectors::gotify",
                %error,
                "Gotify /application lookup failed; application-name filters will not match until the next reconnect",
            );
        }
    }
}

/// Match `msg` against every active subscription and deliver one
/// message per match. Multiple matches fan out to every subscriber.
fn route(host: &dyn GotifyHost, msg: &GotifyMessage) {
    let subs = host.subscriptions();
    let resolved_name = host.app_name(msg.appid);
    // Matching uses the resolved name only; the delivered `app` falls
    // back to the numeric id for display, so an unresolved appid can't
    // match a filter that happens to list that number.
    let display_name = resolved_name.clone().unwrap_or_else(|| msg.appid.to_string());
    for sub in matching_subscriptions(&subs, resolved_name.as_deref(), msg.priority) {
        host.deliver(sub, &display_name, msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_parses_sample_message() {
        let line = r#"{"id":1,"appid":3,"title":"t","message":"m","priority":5,"date":"2026-07-03T09:18:00Z"}"#;
        let msg = normalize(line).expect("parse sample message");
        assert_eq!(msg.appid, 3);
        assert_eq!(msg.priority, 5);
    }

    #[test]
    fn next_backoff_resets_on_healthy_and_escalates_otherwise() {
        // A healthy session resets the ladder to the floor from anywhere.
        assert_eq!(next_backoff(BACKOFF_CAP, true), BACKOFF_START);
        assert_eq!(next_backoff(Duration::from_secs(8), true), BACKOFF_START);
        // An unhealthy/failed connect doubles, capped at BACKOFF_CAP.
        assert_eq!(next_backoff(BACKOFF_START, false), BACKOFF_START * 2);
        assert_eq!(next_backoff(Duration::from_secs(20), false), BACKOFF_CAP);
        assert_eq!(next_backoff(BACKOFF_CAP, false), BACKOFF_CAP);
    }

    #[test]
    fn idle_past_deadline_trips_only_after_the_deadline() {
        let base = Instant::now();
        // At or before the deadline the stream is still considered live.
        assert!(!idle_past_deadline(base, base + Duration::from_secs(1)));
        assert!(!idle_past_deadline(base, base + IDLE_DEADLINE));
        // Strictly past the deadline the path is treated as dead.
        assert!(idle_past_deadline(base, base + IDLE_DEADLINE + Duration::from_secs(1)));
    }

    #[test]
    fn fresh_activity_resets_the_idle_clock() {
        let base = Instant::now();
        // Long after connect, a frame moves last_activity forward; measured
        // from that fresh timestamp the stream is nowhere near the deadline.
        let refreshed = base + Duration::from_secs(10_000);
        assert!(!idle_past_deadline(refreshed, refreshed + Duration::from_secs(1)));
    }

    #[test]
    fn app_name_to_id_maps() {
        let body = r#"[{"id":3,"name":"web-api","token":"A.abc","description":""}]"#;
        let apps: Vec<GotifyApp> = serde_json::from_str(body).expect("parse application list");
        let index = build_app_index(apps);
        assert_eq!(index.get("web-api"), Some(&3));
    }

    #[test]
    fn app_names_extracts_names_in_server_order() {
        let apps = vec![
            GotifyApp { id: 3, name: "web-api".to_owned() },
            GotifyApp { id: 1, name: "Backups".to_owned() },
        ];
        assert_eq!(build_app_names(apps), vec!["web-api".to_owned(), "Backups".to_owned()]);
    }

    #[test]
    fn message_page_parses_ignoring_extras_and_paging() {
        let body = r#"{"messages":[{"id":25,"appid":1,"title":"t","message":"m","priority":4,"date":"2026-07-04T09:18:00Z","extras":{"x":1}}],"paging":{"size":1,"limit":100,"since":25}}"#;
        let page: GotifyMessages = serde_json::from_str(body).expect("parse /message page");
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].appid, 1);
    }

    fn msg(id: u64, appid: u64, priority: u8) -> GotifyMessage {
        GotifyMessage {
            id,
            appid,
            title: format!("t{id}"),
            message: format!("m{id}"),
            priority,
            date: format!("2026-07-04T{id:02}:00:00Z"),
        }
    }

    #[test]
    fn filter_recent_orders_newest_first_and_truncates_to_limit() {
        let index = HashMap::from([(1u64, "CI".to_owned())]);
        // Deliberately out of order; filter_recent must sort newest-first.
        let out =
            filter_recent(vec![msg(1, 1, 5), msg(3, 1, 5), msg(2, 1, 5)], &index, &[], None, 2);
        assert_eq!(out.len(), 2, "truncated to the limit");
        assert_eq!(out[0].title, "t3", "highest id (newest) first");
        assert_eq!(out[1].title, "t2");
    }

    #[test]
    fn filter_recent_applies_app_name_and_priority_filters() {
        let index = HashMap::from([(1u64, "CI".to_owned()), (2u64, "Backups".to_owned())]);
        let msgs = vec![msg(10, 1, 8), msg(9, 2, 8), msg(8, 1, 2)];
        let out = filter_recent(msgs, &index, &["CI".to_owned()], Some(5), 20);
        assert_eq!(out.len(), 1, "only CI at priority >= 5 survives");
        assert_eq!(out[0].app, "CI");
        assert_eq!(out[0].priority, 8);
    }

    #[test]
    fn filter_recent_unresolved_appid_shows_id_but_never_matches_a_named_filter() {
        // appid 7 isn't in the index: its display `app` falls back to "7",
        // but a filter naming "7" must NOT match it - mirrors the
        // resolved-name-only matching in `route`.
        let index = HashMap::from([(1u64, "CI".to_owned())]);
        let unfiltered = filter_recent(vec![msg(5, 7, 9)], &index, &[], None, 20);
        assert_eq!(unfiltered[0].app, "7", "unresolved appid displays as its id string");
        let named = filter_recent(vec![msg(5, 7, 9)], &index, &["7".to_owned()], None, 20);
        assert!(named.is_empty(), "a numeric-string filter can't match an unresolved appid");
    }

    fn sub(applications: &[&str], min_priority: Option<u8>) -> GotifySubscription {
        GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: "p".to_owned(),
            team_role: None,
            applications: applications.iter().map(|s| (*s).to_owned()).collect(),
            min_priority,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// Every filter dimension at once: the priority floor, the app-name
    /// set (matching ANY listed name), and the match-any empty set.
    #[test]
    fn matching_subscriptions_applies_priority_and_application_filters() {
        let subs = [sub(&["alerts"], None), sub(&[], Some(5)), sub(&["alerts", "backups"], None)];
        let matches = |resolved: Option<&str>, priority| {
            matching_subscriptions(&subs, resolved, priority)
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>()
        };

        // alerts at priority 2: the app-filter subs match; the priority
        // floor does not (2 < 5).
        assert_eq!(matches(Some("alerts"), 2), [subs[0].id, subs[2].id]);
        // The SECOND listed name matches the same set as the first, so a
        // single-element comparison can't pass for the any-of filter.
        assert_eq!(matches(Some("backups"), 1), [subs[2].id]);
        // Below the floor only the match-any priority set survives.
        assert_eq!(matches(Some("alerts"), 5), [subs[0].id, subs[1].id, subs[2].id]);
        // An app outside every listed set leaves only the empty filter set.
        assert_eq!(matches(Some("ci"), 9), [subs[1].id]);
    }

    /// The seam the route matcher pins: matching uses the RESOLVED app
    /// name only, so an unresolved appid must not match a filter that
    /// happens to list its numeric id as a string.
    #[test]
    fn matching_subscriptions_unresolved_appid_never_matches_a_numeric_filter() {
        let subs = [sub(&["5"], None), sub(&[], None)];
        let matches = matching_subscriptions(&subs, None, 1);
        assert_eq!(matches.iter().map(|s| s.id).collect::<Vec<_>>(), [subs[1].id]);
    }

    /// Multiple matching subscriptions all come back: the caller fans a
    /// message out to every subscriber.
    #[test]
    fn matching_subscriptions_fans_out_across_projects() {
        let subs = [sub(&[], None), sub(&[], None), sub(&[], Some(9))];
        assert_eq!(matching_subscriptions(&subs, None, 5).len(), 2);
    }

    /// Records `deliver` calls: `(project, team_role, app, priority)`.
    type Delivery = (String, Option<String>, String, u8);

    struct MockHost {
        subs: Vec<GotifySubscription>,
        index: HashMap<u64, String>,
        delivered: std::sync::Mutex<Vec<Delivery>>,
    }

    impl GotifyHost for MockHost {
        fn http_client(&self, _timeout: Duration) -> Result<reqwest::Client, String> {
            Err("unused by route".to_owned())
        }

        fn subscriptions(&self) -> Vec<GotifySubscription> {
            self.subs.clone()
        }

        fn app_name(&self, appid: u64) -> Option<String> {
            self.index.get(&appid).cloned()
        }

        fn store_app_index(&self, _index: HashMap<String, u64>) {}

        fn set_connected(&self, _connected: bool) {}

        fn deliver(&self, subscription: &GotifySubscription, app: &str, message: &GotifyMessage) {
            self.delivered.lock().expect("mock lock").push((
                subscription.project.clone(),
                subscription.team_role.clone(),
                app.to_owned(),
                message.priority,
            ));
        }
    }

    /// The seam between the matcher and the host: `route` matches on the
    /// RESOLVED app name only, so an unresolved appid never satisfies a
    /// filter listing its numeric id, while the delivered `app` still
    /// falls back to that id string for display.
    #[test]
    fn route_matches_on_resolved_names_and_falls_back_to_the_id_for_display() {
        let numeric_filter = sub(&["5"], None);
        let match_any = sub(&[], None);
        let host = MockHost {
            subs: vec![numeric_filter, match_any],
            index: HashMap::new(),
            delivered: std::sync::Mutex::new(Vec::new()),
        };

        route(&host, &msg(1, 5, 1));

        let delivered = host.delivered.lock().expect("mock lock").clone();
        assert_eq!(
            delivered.len(),
            1,
            "an unresolved appid matches only the empty filter set: {delivered:?}",
        );
        assert_eq!(delivered[0].2, "5", "the delivered app falls back to the id string");
    }

    /// Every matching subscriber receives one delivery, carrying its own
    /// project + team role.
    #[test]
    fn route_fans_out_to_every_matching_subscriber() {
        let lead = sub(&["alerts"], None);
        let mut worker = sub(&[], Some(5));
        worker.team_role = Some("reviewer".to_owned());
        let host = MockHost {
            subs: vec![lead, worker],
            index: HashMap::from([(3u64, "alerts".to_owned())]),
            delivered: std::sync::Mutex::new(Vec::new()),
        };

        // Priority 2 clears the app filter but not the floor.
        route(&host, &msg(1, 3, 2));
        assert_eq!(host.delivered.lock().expect("mock lock").len(), 1);

        // Priority 5 clears both, so both subscribers deliver.
        route(&host, &msg(2, 3, 5));
        let delivered = host.delivered.lock().expect("mock lock").clone();
        assert_eq!(delivered.len(), 3, "one more delivery on the second message");
        assert_eq!(delivered[1].0, "p");
        assert_eq!(delivered[2].1.as_deref(), Some("reviewer"));
    }
}
