//! Background runtime for [`Client`](crate::Client) - owns the
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

/// Outcome of one outbound `control_request` - either the success
/// payload (the inner `response` JSON) or a typed error.
pub(crate) type ControlOutcome = Result<serde_json::Value, Error>;

/// In-flight outbound `control_request`s waiting on responses.
pub(crate) type PendingControls = Arc<Mutex<HashMap<String, oneshot::Sender<ControlOutcome>>>>;

/// In-flight inbound `control_request` dispatch tasks. Keyed by
/// `request_id` so a subsequent `control_cancel_request` can abort
/// the matching task instead of letting the slow callback finish and
/// write back a `control_response` the CLI has already moved past.
pub(crate) type InflightDispatches = Arc<Mutex<HashMap<String, JoinHandle<()>>>>;

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
    use tracing::Instrument;
    let inflight: InflightDispatches = Arc::new(Mutex::new(HashMap::new()));
    let span = tracing::info_span!("forge_sdk::control_reader");
    tokio::spawn(
        async move {
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
                                    &inflight,
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
        }
        .instrument(span),
    )
}

/// Process one decoded line. Returns `false` when the read loop should
/// exit (events channel closed, terminal error).
async fn handle_line(
    dispatch: &ControlDispatchHandle,
    pending_controls: &PendingControls,
    inflight: &InflightDispatches,
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
            let dispatch_clone = dispatch.clone();
            let inflight_clone = Arc::clone(inflight);
            let request_id = req.request_id.clone();
            let request_id_for_task = request_id.clone();
            // Park the task on a oneshot before doing work so the
            // parent can register the JoinHandle in `inflight` before
            // the task races ahead and removes itself from a still-
            // empty map (which would leak a completed handle
            // permanently). Once the parent releases the gate, the
            // task drains it and runs.
            let (gate_tx, gate_rx) = oneshot::channel::<()>();
            let handle = tokio::spawn(async move {
                let _ = gate_rx.await;
                let result = dispatch_clone.dispatch(req).await;
                inflight_clone.lock().await.remove(&request_id_for_task);
                if let Err(e) = result {
                    tracing::error!(
                        target: crate::logging::targets::SDK_READER,
                        error = %e,
                        "control_dispatch failed",
                    );
                }
            });
            inflight.lock().await.insert(request_id, handle);
            // Release the gate after the registration is visible.
            let _ = gate_tx.send(());
            true
        }
        Ok(DecodedLine::ControlCancel { request_id }) => {
            if let Some(handle) = inflight.lock().await.remove(&request_id) {
                handle.abort();
                tracing::debug!(
                    target: crate::logging::targets::SDK_READER,
                    %request_id,
                    "control_cancel_request: dispatch task aborted",
                );
            } else {
                tracing::debug!(
                    target: crate::logging::targets::SDK_READER,
                    %request_id,
                    "control_cancel_request: no in-flight dispatch (already completed)",
                );
            }
            true
        }
        Ok(DecodedLine::ControlResponse { request_id, raw: value }) => {
            let resp_subtype =
                value.pointer("/response/subtype").and_then(serde_json::Value::as_str);
            let outcome = if resp_subtype == Some("success") {
                Ok(value.pointer("/response/response").cloned().unwrap_or(serde_json::Value::Null))
            } else {
                let err = value
                    .pointer("/response/error")
                    .and_then(serde_json::Value::as_str)
                    .map_or_else(
                        || {
                            format!(
                                "no `error` string field; full response: {}",
                                value
                                    .pointer("/response")
                                    .map_or_else(|| "<missing>".to_string(), ToString::to_string,)
                            )
                        },
                        ToString::to_string,
                    );
                Err(Error::message_parse(format!("control failed: {err}")))
            };
            let mut pending = pending_controls.lock().await;
            if let Some(tx) = pending.remove(&request_id) {
                let _ = tx.send(outcome);
            } else {
                tracing::warn!(
                    target: crate::logging::targets::SDK_READER,
                    %request_id,
                    "unexpected control_response - dropping",
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
                "unknown top-level stream-json type - surfacing as Message::Unknown",
            );
            events_tx.send(Ok(Message::Unknown { type_str, raw })).is_ok()
        }
        Ok(DecodedLine::ToolProgress(progress)) => {
            // Dropped on purpose: informational only, forge's own tool
            // lifecycle rendering covers it. Debug, not warn - a 30s
            // cadence at warn is 10MB of log rotation per session-hour.
            tracing::debug!(
                target: crate::logging::targets::SDK_READER,
                tool_name = %progress.tool_name,
                elapsed_time_seconds = progress.elapsed_time_seconds,
                heartbeat = progress.heartbeat,
                line = line_number,
                "tool_progress heartbeat dropped",
            );
            true
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
        // warn, not debug: `sdk.reader` is not raised to debug by the
        // default directives or any diagnostics preset, so a debug event
        // here is dropped at the filter on every shipped configuration.
        tracing::warn!(
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

/// Shared session-id container - held by [`Client`](crate::Client),
/// [`ControlDispatchHandle`], and the reader task. Reader updates it
/// as messages arrive; consumers read the current value.
pub(crate) type SharedSessionId = Arc<RwLock<String>>;

/// Build a fresh empty session-id holder.
pub(crate) fn new_shared_session_id() -> SharedSessionId {
    Arc::new(RwLock::new(String::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::process::SharedWriter;

    /// A heartbeat is dropped: the read loop continues and nothing
    /// reaches the events channel. Mutating the arm to `return false`
    /// would end the session on every 30-second tick - the blast
    /// radius this test exists to pin.
    #[tokio::test]
    async fn a_tool_progress_heartbeat_is_dropped_without_ending_the_stream() {
        let (writer, _lines) = SharedWriter::test_stub();
        let dispatch = ControlDispatchHandle::new(
            Arc::new(writer),
            None,
            None,
            crate::mcp::orchestration::McpHosts::new(Vec::new(), HashMap::new()),
            HashMap::new(),
            new_shared_session_id(),
        );
        let pending: PendingControls = Arc::new(Mutex::new(HashMap::new()));
        let inflight: InflightDispatches = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();

        let line = r#"{"type":"tool_progress","tool_use_id":"toolu_01QhFqNDEgKeskhhiYpzeHnL-heartbeat-0","tool_name":"Bash","parent_tool_use_id":"toolu_01QhFqNDEgKeskhhiYpzeHnL","elapsed_time_seconds":30,"heartbeat":true,"session_id":"s","uuid":"u"}"#;
        let keep_going = handle_line(&dispatch, &pending, &inflight, &events_tx, 40, line).await;

        assert!(keep_going, "a heartbeat must not end the read loop");
        assert!(
            events_rx.try_recv().is_err(),
            "a heartbeat must not surface an event to the agent"
        );
    }
}
