//! `session.*` method handlers.
//!
//! Each handler is a thin proxy over the session's actor task — they
//! enqueue a [`Command`] on the session's mpsc and `.await` the actor's
//! reply. The actor (spawned at `session.spawn`) is the sole owner of
//! the [`forge_sdk::Client`]; locking the [`Client`] from multiple
//! tasks would deadlock because [`forge_sdk::Client::next_event`]
//! holds `&mut self` across subprocess I/O.

use forge_sdk::{Client, Options};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;
use tracing::info;
use uuid::Uuid;

use crate::Error;
use crate::registry::DaemonState;
use crate::session_state::{Command, SessionHandle, SessionId};

/// Result of `session.spawn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SpawnResult {
    /// Daemon-minted id for the freshly-spawned session.
    pub session_id: SessionId,
}

/// `session.spawn` — create a new claude session inside the daemon and
/// boot its actor task.
///
/// Uses [`crate::bridged_transport::BridgedTransport`] rather than
/// [`forge_sdk::Client::spawn`] so the actor can [`tokio::select!`]
/// between [`forge_sdk::Client::next_event`] reads and command-driven
/// writes without holding the [`Client`] lock across blocking I/O. See
/// the bridged-transport module docs for the full rationale.
///
/// # Errors
///
/// Bubbles `forge_sdk::Error` for spawn failures.
pub async fn spawn(state: &DaemonState, options: Options) -> Result<SpawnResult, Error> {
    let bridge = crate::bridged_transport::BridgedTransport::spawn(&options)
        .await
        .map_err(Error::Sdk)?;
    let client = Client::spawn_with_transport(options, Box::new(bridge))
        .await
        .map_err(Error::Sdk)?;
    let session_id = SessionId(format!("sess_{}", Uuid::new_v4()));
    let (handle, rx) = state.register_session(session_id.clone());
    spawn_session_actor(state.clone(), &handle, client, rx);
    info!(session_id = %session_id.0, "session spawned");
    Ok(SpawnResult { session_id })
}

/// Extract a stable per-message identifier when one exists. Used as the
/// `event_id` field on `session.event` notifications.
///
/// `forge_sdk::Message` carries a `uuid` field on most variants but not all
/// — `Error`, `Unknown`, `System`, plus `Assistant`/`User`/`Result` when
/// the CLI hasn't been configured to emit them. Callers should treat the
/// empty string as "no id".
#[must_use]
pub fn message_event_id(msg: &forge_sdk::Message) -> &str {
    use forge_sdk::Message;
    match msg {
        Message::Assistant { uuid, .. }
        | Message::User { uuid, .. }
        | Message::Result { uuid, .. } => uuid.as_deref().unwrap_or(""),
        Message::TaskStarted { uuid, .. }
        | Message::TaskProgress { uuid, .. }
        | Message::TaskNotification { uuid, .. }
        | Message::RateLimitEvent { uuid, .. }
        | Message::StreamEvent { uuid, .. } => uuid.as_str(),
        // `System`, `Error`, `Unknown`, plus any future `non_exhaustive`
        // variants — fall through to the empty-string sentinel.
        _ => "",
    }
}

/// Wire-shape parameters for `session.send_user_message`.
#[derive(Debug, Clone, Deserialize)]
pub struct SendUserMessageParams {
    /// Session id minted by `session.spawn`.
    pub session_id: SessionId,
    /// The prompt text to forward to claude.
    pub prompt: String,
}

/// `session.send_user_message` — forward a prompt to the underlying claude.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn send_user_message(
    state: &DaemonState,
    session_id: &SessionId,
    prompt: &str,
) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::SendUserMessage {
            prompt: prompt.to_owned(),
            reply,
        })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}

/// Result of `session.subscribe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubscribeResult {
    /// Number of historical messages replayed before the live cursor.
    /// M2 stub always reports 0; replay buffer lands in M3.
    pub replayed: usize,
    /// True iff the subscription is now receiving live events.
    pub live: bool,
    /// Pending prompts queued for the subscriber to answer (M4); empty in M2.
    pub pending_prompts: Vec<Value>,
}

/// Wire-shape parameters for `session.subscribe`.
#[derive(Debug, Clone, Deserialize)]
pub struct SubscribeParams {
    /// Session to subscribe to.
    pub session_id: SessionId,
    /// Resume cursor (M3); ignored in M2.
    #[serde(default)]
    pub since: Option<String>,
}

/// `session.subscribe` — register `conn` as a subscriber of `session_id`.
/// The session's actor task is already running (spawned at `session.spawn`);
/// this just adds `conn` to the subscriber list so the broadcast helper
/// fans events to it.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown.
pub fn subscribe(
    state: &DaemonState,
    conn: &crate::connection::Connection,
    session_id: &SessionId,
    _since: Option<String>,
) -> Result<SubscribeResult, Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;

    {
        let mut subs = handle.subscribers.lock();
        if !subs.contains(&conn.id) {
            subs.push(conn.id.clone());
        }
        let mut primary = handle.primary.lock();
        if primary.is_none() {
            *primary = Some(conn.id.clone());
        }
    }

    Ok(SubscribeResult {
        replayed: 0,
        live: true,
        pending_prompts: Vec::new(),
    })
}

/// Wire-shape parameters for `session.unsubscribe`.
#[derive(Debug, Clone, Deserialize)]
pub struct UnsubscribeParams {
    /// Session to detach from.
    pub session_id: SessionId,
}

/// `session.unsubscribe` — remove `conn` from the subscriber list. Clears
/// the primary slot if `conn` held it.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown.
pub fn unsubscribe(
    state: &DaemonState,
    conn: &crate::connection::Connection,
    session_id: &SessionId,
) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    handle.subscribers.lock().retain(|c| c != &conn.id);
    let mut primary = handle.primary.lock();
    if primary.as_ref() == Some(&conn.id) {
        *primary = None;
    }
    Ok(())
}

/// Wire-shape parameters for `session.disconnect`.
#[derive(Debug, Clone, Deserialize)]
pub struct DisconnectParams {
    /// Session to tear down.
    pub session_id: SessionId,
}

/// `session.disconnect` — ask the actor to consume its [`Client`] and call
/// [`Client::disconnect`]. The actor handles unregistering the session
/// once the call returns.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn disconnect(state: &DaemonState, session_id: &SessionId) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::Disconnect { reply })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}

/// Wire-shape parameters for `session.end_input`.
#[derive(Debug, Clone, Deserialize)]
pub struct EndInputParams {
    /// Session whose stdin should be closed.
    pub session_id: SessionId,
}

/// `session.end_input` — close the subprocess's stdin so it can flush its
/// final result frame and exit. Does NOT unregister the session; the
/// actor's read loop emits `session.closed` when `next_event` returns
/// `Ok(None)` / `Err(_)` (M2.6).
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn end_input(state: &DaemonState, session_id: &SessionId) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::EndInput { reply })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}

/// Spawn the actor task that exclusively owns `client` for the lifetime of
/// the session. The actor `select!`s between:
///
/// 1. Inbound [`Command`]s from dispatch handlers — `SendUserMessage`,
///    `EndInput`, `Disconnect` — and runs them on the [`Client`].
/// 2. Outbound `next_event` calls — fans each [`forge_sdk::Message`] to
///    the session's subscribers as a `session.event` notification.
///
/// On a terminal frame (`Message::Result`), `Ok(None)` from `next_event`,
/// or any transport error, the actor emits a `session.closed` notification
/// and unregisters the session from the daemon (M2.6).
fn spawn_session_actor(
    state: DaemonState,
    handle: &SessionHandle,
    mut client: Client,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let session_id = handle.id.clone();
    tokio::spawn(async move {
        let reason: &'static str = loop {
            tokio::select! {
                biased;
                cmd = commands.recv() => {
                    let Some(cmd) = cmd else {
                        // Senders all dropped — session is being torn down.
                        break "actor_idle";
                    };
                    match cmd {
                        Command::SendUserMessage { prompt, reply } => {
                            let r = client.send_user_message(&prompt).await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::EndInput { reply } => {
                            let r = client.end_input().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::Disconnect { reply } => {
                            let r = client.disconnect().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                            break "disconnect";
                        }
                    }
                }
                next = client.next_event() => {
                    match next {
                        Ok(Some(msg)) => {
                            let is_terminal = matches!(msg, forge_sdk::Message::Result { .. });
                            let event_id = message_event_id(&msg).to_owned();
                            let frame = crate::connection::Outbound::Notification(
                                crate::jsonrpc::Notification::new(
                                    "session.event",
                                    serde_json::json!({
                                        "session_id": session_id.0,
                                        "event_id": event_id,
                                        "message": msg,
                                    }),
                                ),
                            );
                            crate::broadcast::fanout(&state, &session_id, &frame);
                            if is_terminal {
                                break "result_frame";
                            }
                        }
                        Ok(None) => break "disconnected",
                        Err(e) => {
                            tracing::warn!(session_id = %session_id.0, error = %e, "next_event error");
                            break "error";
                        }
                    }
                }
            }
        };

        // Emit session.closed to all subscribers.
        let closed = crate::connection::Outbound::Notification(crate::jsonrpc::Notification::new(
            "session.closed",
            serde_json::json!({
                "session_id": session_id.0,
                "reason": reason,
            }),
        ));
        crate::broadcast::fanout(&state, &session_id, &closed);

        // Unregister the session — frees state, decrements active_sessions.
        state.unregister_session(&session_id);
    });
}
