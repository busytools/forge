//! Background runtime for [`Client`](crate::Client) — owns the
//! subprocess reader, decodes incoming frames, dispatches inbound
//! `control_request`s on detached tasks, and routes outbound
//! `control_response`s to the [`pending_controls`] map so [`send_control`]
//! callers can `await` their typed reply.
//!
//! ## Lifecycle
//!
//! 1. [`Client::spawn`] runs the init handshake inline (sends the
//!    `initialize` `control_request`, drains the response, captures
//!    any pre-init Messages).
//! 2. Once init completes, [`spawn_reader_task`] takes ownership of
//!    the [`Subprocess`], the dispatch handle, the events channel,
//!    and any pre-init Messages.
//! 3. The reader task pre-pumps the buffered messages, then loops on
//!    `subprocess.read_line()` until shutdown / EOF / I/O error.
//! 4. On exit, the reader task closes the subprocess and drains
//!    `pending_controls` with an EOF error so blocked
//!    [`send_control`] callers wake up.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::Error;
use crate::client::ControlDispatchHandle;
use crate::transport::codec::{DecodedLine, decode_dispatch};
use crate::transport::process::Subprocess;
use forge_primitives::Message;

/// Outcome of one outbound `control_request` — either the success
/// payload (the inner `response` JSON) or a typed error.
pub(crate) type ControlOutcome = Result<serde_json::Value, Error>;

/// In-flight outbound `control_request`s waiting on responses.
pub(crate) type PendingControls = Arc<Mutex<HashMap<String, oneshot::Sender<ControlOutcome>>>>;

/// Spawn the post-init reader task.
///
/// Pre-pumps `pre_init_messages` into `events_tx` first, then runs the
/// main read loop. On shutdown signal (or EOF / I/O error / closed
/// events channel), closes the transport and drains pending controls
/// with an EOF error so blocked `send_control` callers wake up.
pub(crate) fn spawn_reader_task(
    mut subprocess: Subprocess,
    dispatch: ControlDispatchHandle,
    pending_controls: PendingControls,
    events_tx: mpsc::UnboundedSender<Result<Message, Error>>,
    pre_init_messages: Vec<Message>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Drain anything captured during init.
        for msg in pre_init_messages {
            dispatch.capture_session_id_from(&msg);
            if events_tx.send(Ok(msg)).is_err() {
                close_subprocess(&mut subprocess).await;
                drain_pending(&pending_controls).await;
                return;
            }
        }

        let mut line_number: u64 = 0;
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => break,
                line = subprocess.read_line() => {
                    match line {
                        Ok(Some(line)) => {
                            line_number += 1;
                            if !handle_line(
                                &dispatch,
                                &pending_controls,
                                &events_tx,
                                line_number,
                                &line,
                            )
                            .await
                            {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            let err_text = e.to_string();
                            if events_tx.send(Err(e)).is_err() {
                                tracing::warn!(
                                    target: crate::logging::targets::SDK_READER,
                                    error = %err_text,
                                    "events channel closed; transport error dropped",
                                );
                            }
                            break;
                        }
                    }
                }
            }
        }

        close_subprocess(&mut subprocess).await;
        drain_pending(&pending_controls).await;
    })
}

/// Process one decoded line. Returns `false` when the read loop should
/// exit (events channel closed, terminal error).
async fn handle_line(
    dispatch: &ControlDispatchHandle,
    pending_controls: &PendingControls,
    events_tx: &mpsc::UnboundedSender<Result<Message, Error>>,
    line_number: u64,
    line: &str,
) -> bool {
    match decode_dispatch(line, line_number) {
        Ok(DecodedLine::Message(msg)) => {
            dispatch.capture_session_id_from(&msg);
            events_tx.send(Ok(msg)).is_ok()
        }
        Ok(DecodedLine::Control(req)) => {
            let dispatch = dispatch.clone();
            tokio::spawn(async move {
                if let Err(e) = dispatch.dispatch(req).await {
                    tracing::warn!(
                        target: crate::logging::targets::SDK_READER,
                        error = %e,
                        "control_dispatch failed",
                    );
                }
            });
            true
        }
        Ok(DecodedLine::ControlCancel { request_id }) => {
            tracing::debug!(
                target: crate::logging::targets::SDK_READER,
                %request_id,
                "control_cancel_request received; nothing to cancel",
            );
            true
        }
        Ok(DecodedLine::ControlResponse { request_id, .. }) => {
            // Re-decode to extract success/error payload — the
            // DecodedLine variant only carries request_id, not the
            // body, so peek at the raw line again.
            let outcome = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => {
                    let resp_subtype =
                        value.pointer("/response/subtype").and_then(serde_json::Value::as_str);
                    if resp_subtype == Some("success") {
                        Ok(value
                            .pointer("/response/response")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null))
                    } else {
                        let err = value
                            .pointer("/response/error")
                            .and_then(serde_json::Value::as_str)
                            .map_or_else(
                                || {
                                    format!(
                                        "no `error` string field; full response: {}",
                                        value.pointer("/response").map_or_else(
                                            || "<missing>".to_string(),
                                            ToString::to_string,
                                        )
                                    )
                                },
                                ToString::to_string,
                            );
                        Err(Error::message_parse(format!("control failed: {err}")))
                    }
                }
                Err(source) => Err(Error::JsonDecode { line: line_number, source }),
            };
            let mut pending = pending_controls.lock().await;
            if let Some(tx) = pending.remove(&request_id) {
                let _ = tx.send(outcome);
            } else {
                tracing::warn!(
                    target: crate::logging::targets::SDK_READER,
                    %request_id,
                    "unexpected control_response — dropping",
                );
            }
            true
        }
        Ok(DecodedLine::Unknown { type_str, raw }) => {
            tracing::warn!(
                target: crate::logging::targets::SDK_READER,
                type = %type_str,
                raw = %raw,
                line = line_number,
                "unknown top-level stream-json type — surfacing as Message::Unknown",
            );
            events_tx.send(Ok(Message::Unknown { type_str, raw })).is_ok()
        }
        Err(e) => {
            let err_text = e.to_string();
            if events_tx.send(Err(e)).is_err() {
                tracing::warn!(
                    target: crate::logging::targets::SDK_READER,
                    error = %err_text,
                    line_number,
                    "events channel closed; decode error dropped",
                );
            }
            false
        }
    }
}

async fn close_subprocess(subprocess: &mut Subprocess) {
    if let Err(e) = subprocess.close().await {
        tracing::debug!(
            target: crate::logging::targets::SDK_READER,
            error = %e,
            "reader task: subprocess close error",
        );
    }
}

async fn drain_pending(pending_controls: &PendingControls) {
    let mut pending = pending_controls.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(Error::Connection {
            reason: "subprocess closed before control_response".into(),
        }));
    }
}

/// Shared session-id container — held by [`Client`](crate::Client),
/// [`ControlDispatchHandle`], and the reader task. Reader updates it
/// as messages arrive; consumers read the current value.
pub(crate) type SharedSessionId = Arc<RwLock<String>>;

/// Build a fresh empty session-id holder.
#[must_use]
pub(crate) fn new_shared_session_id() -> SharedSessionId {
    Arc::new(RwLock::new(String::new()))
}
