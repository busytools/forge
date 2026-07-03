//! Gotify WebSocket client + `/application` name->id resolution.
//!
//! Lives under `env` (network-side environment state the agent
//! observes) even though the Gotify server is external:
//! forge-workspace owns the long-lived [`run`] task and consumes the
//! [`GotifyEvent`]s it emits, which is a legal agent->workspace flow.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use forge_primitives::{GotifyConfig, GotifyMessage};
use futures::StreamExt;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::logging::targets::GOTIFY;

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

/// Per-request timeout for the `/application` lookup - keep a slow or
/// unreachable server from stalling subsystem start.
const APP_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Connection-lifecycle + message events from the stream task to the
/// workspace subsystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GotifyEvent {
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

/// Fetch the server's application list and fold it into a name->appid
/// map. Inbound stream messages carry the numeric appid, so the map
/// resolves an `application` name filter to the id to match against.
pub async fn app_index(cfg: &GotifyConfig) -> anyhow::Result<HashMap<String, u64>> {
    let client =
        crate::http_trust::with_extra_roots(reqwest::Client::builder().timeout(APP_LOOKUP_TIMEOUT))
            .build()
            .context("build Gotify http client")?;
    let url = format!("{}/application", cfg.url.trim_end_matches('/'));
    let apps: Vec<GotifyApp> = client
        .get(url)
        .header("X-Gotify-Key", &cfg.client_token)
        .send()
        .await
        .context("GET /application")?
        .error_for_status()
        .context("/application returned an error status")?
        .json()
        .await
        .context("parse /application body")?;
    Ok(build_app_index(apps))
}

fn build_app_index(apps: Vec<GotifyApp>) -> HashMap<String, u64> {
    apps.into_iter().map(|app| (app.name, app.id)).collect()
}

/// Parse a raw stream Text frame into a typed [`GotifyMessage`].
fn normalize(text: &str) -> anyhow::Result<GotifyMessage> {
    serde_json::from_str(text).context("parse Gotify stream message")
}

/// An open Gotify receive stream.
pub struct GotifyStream {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl GotifyStream {
    /// Open `{ws,wss}://<host>/stream?token=<client_token>` - `wss` for
    /// an `https` url, `ws` for `http`.
    pub async fn connect(cfg: &GotifyConfig) -> anyhow::Result<Self> {
        let url = stream_url(cfg)?;
        let (ws, _resp) = connect_async(url.as_str()).await.context("connect Gotify stream")?;
        Ok(Self { ws })
    }

    /// The next decoded message, or `None` when the stream ends or
    /// errors. Ping/Pong/Binary/Close frames are skipped; a Text frame
    /// that fails to parse is skipped with a warn.
    pub async fn recv(&mut self) -> Option<GotifyMessage> {
        while let Some(frame) = self.ws.next().await {
            match frame {
                Ok(Message::Text(text)) => match normalize(text.as_str()) {
                    Ok(msg) => return Some(msg),
                    Err(error) => {
                        tracing::warn!(target: GOTIFY, %error, "skipping unparseable stream frame");
                    }
                },
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(target: GOTIFY, %error, "Gotify stream read error");
                    return None;
                }
            }
        }
        None
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

/// Long-lived reconnect loop: connect (emit [`GotifyEvent::Connected`]),
/// forward each message as [`GotifyEvent::Message`], and on drop/error
/// emit [`GotifyEvent::Disconnected`] then retry with exponential backoff.
/// The backoff resets to the floor only after a healthy session (one that
/// stayed up past [`MIN_HEALTHY_UPTIME`]); a fast drop or failed dial keeps
/// it escalating. Exits when `shutdown` fires or the sender is dropped.
pub async fn run(
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
            Ok(Ok(mut stream)) => {
                let connected_at = tokio::time::Instant::now();
                if tx.send(GotifyEvent::Connected).await.is_err() {
                    return;
                }
                loop {
                    tokio::select! {
                        _ = &mut shutdown => return,
                        msg = stream.recv() => match msg {
                            Some(m) => {
                                if tx.send(GotifyEvent::Message(m)).await.is_err() {
                                    return;
                                }
                            }
                            None => break,
                        }
                    }
                }
                let _ = tx.send(GotifyEvent::Disconnected).await;
                connected_at.elapsed() >= MIN_HEALTHY_UPTIME
            }
            Ok(Err(error)) => {
                tracing::warn!(target: GOTIFY, %error, "Gotify connect failed; backing off");
                let _ = tx.send(GotifyEvent::Disconnected).await;
                false
            }
            Err(_) => {
                tracing::warn!(
                    target: GOTIFY,
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
    fn app_name_to_id_maps() {
        let body = r#"[{"id":3,"name":"trader-cc","token":"A.abc","description":""}]"#;
        let apps: Vec<GotifyApp> = serde_json::from_str(body).expect("parse application list");
        let index = build_app_index(apps);
        assert_eq!(index.get("trader-cc"), Some(&3));
    }
}
