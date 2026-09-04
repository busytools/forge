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

/// Consecutive skips of unparsable lines. At
/// [`UNPARSABLE_ESCALATE_AFTER`] in a row the stream is not speaking
/// stream-json, and the near-100%-malformed session would otherwise
/// present to the user as a silent hang.
#[derive(Default)]
pub(crate) struct SkipCounters {
    consecutive: u64,
    total: u64,
}

/// Consecutive unparsable lines after which the reader escalates once
/// to a session-visible error. A sparse bad frame still just skips.
const UNPARSABLE_ESCALATE_AFTER: u64 = 50;

/// What [`handle_line`] decided about one decoded line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LineOutcome {
    /// Handled normally; keep reading.
    Continue,
    /// The line was unparsable and was skipped; keep reading.
    Skipped,
    /// The events channel closed; the read loop should exit.
    Stop,
}

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
            let mut counters = SkipCounters::default();
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    line = subprocess.read_line() => {
                        match line {
                            Ok(Some(line)) => {
                                line_number += 1;
                                if handle_line(
                                    &dispatch,
                                    &pending_controls,
                                    &inflight,
                                    &events_tx,
                                    line_number,
                                    &line,
                                    &mut counters,
                                )
                                .await
                                == LineOutcome::Stop
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

            if counters.total > 0 {
                tracing::warn!(
                    target: crate::logging::targets::SDK_READER,
                    total = counters.total,
                    "stream ended with unparsable lines skipped",
                );
            }

            close_subprocess(&mut subprocess).await;
            drain_pending(&pending_controls).await;
        }
        .instrument(span),
    )
}

/// Process one decoded line. Returns [`LineOutcome::Stop`] only when
/// the events channel has closed and the read loop should exit; a
/// line that fails to decode is skipped and the loop continues.
#[allow(clippy::too_many_arguments)]
async fn handle_line(
    dispatch: &ControlDispatchHandle,
    pending_controls: &PendingControls,
    inflight: &InflightDispatches,
    events_tx: &mpsc::UnboundedSender<Result<Message, Error>>,
    line_number: u64,
    line: &str,
    counters: &mut SkipCounters,
) -> LineOutcome {
    let outcome = match decode_dispatch(line, line_number) {
        DecodedLine::Message(msg) => {
            dispatch.capture_session_id_from(&msg);
            if events_tx.send(Ok(msg)).is_ok() { LineOutcome::Continue } else { LineOutcome::Stop }
        }
        DecodedLine::Malformed { line: line_no, reason } => {
            counters.consecutive += 1;
            counters.total += 1;
            // A recognised control_request with one bad field must
            // still be answered, or the CLI blocks on it forever.
            // Detached like the dispatch path: a full stdin pipe must
            // not stall reads.
            if let Some(request_id) = control_request_id(line) {
                let dispatch = dispatch.clone();
                let reason = reason.clone();
                tokio::spawn(async move {
                    dispatch.write_error_response(&request_id, &reason).await;
                });
            }
            let raw = excerpt(line, 160);
            tracing::warn!(
                target: crate::logging::targets::SDK_READER,
                line = line_no,
                reason = %reason,
                raw = %raw,
                "unparsable stream-json line - skipping, session continues",
            );
            if counters.consecutive == UNPARSABLE_ESCALATE_AFTER {
                tracing::error!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    consecutive = counters.consecutive,
                    "subprocess is not speaking stream-json - surfacing a session failure",
                );
                let _ = events_tx.send(Err(Error::Connection {
                    reason: format!(
                        "{} consecutive unparsable stream-json lines; \
                         the subprocess is not speaking stream-json",
                        counters.consecutive
                    ),
                }));
            }
            LineOutcome::Skipped
        }
        DecodedLine::Control(req) => {
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
            LineOutcome::Continue
        }
        DecodedLine::ControlCancel { request_id } => {
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
            LineOutcome::Continue
        }
        DecodedLine::ControlResponse { request_id, raw: value } => {
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
            LineOutcome::Continue
        }
        DecodedLine::Unknown { type_str, raw } => {
            tracing::warn!(
                target: crate::logging::targets::SDK_READER,
                type = %type_str,
                raw = %raw,
                line = line_number,
                "unknown top-level stream-json type - surfacing as Message::Unknown",
            );
            if events_tx.send(Ok(Message::Unknown { type_str, raw })).is_ok() {
                LineOutcome::Continue
            } else {
                LineOutcome::Stop
            }
        }
        DecodedLine::ToolProgress(progress) => {
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
            LineOutcome::Continue
        }
    };
    // Only a skip keeps the run alive; anything else resets it.
    if outcome != LineOutcome::Skipped {
        counters.consecutive = 0;
    }
    outcome
}

/// First `max_chars` of `line` for logs and error strings - a corrupt
/// line can be arbitrarily large.
pub(crate) fn excerpt(line: &str, max_chars: usize) -> String {
    line.chars().take(max_chars).collect()
}

/// `request_id` of `line` when it is a `control_request`, else `None`.
fn control_request_id(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("control_request") {
        return None;
    }
    value.pointer("/request_id").and_then(serde_json::Value::as_str).map(str::to_string)
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
        let mut counters = SkipCounters::default();
        let outcome =
            handle_line(&dispatch, &pending, &inflight, &events_tx, 40, line, &mut counters).await;

        assert_eq!(outcome, LineOutcome::Continue, "a heartbeat must not end the read loop");
        assert!(
            events_rx.try_recv().is_err(),
            "a heartbeat must not surface an event to the agent"
        );
    }

    /// A line that fails to decode is skipped: the read loop continues
    /// and nothing reaches the events channel, and the next valid frame
    /// still arrives. A decode error used to end the whole session
    /// here, one bad field from a non-Anthropic backend included.
    #[tokio::test]
    async fn a_malformed_line_is_skipped_and_the_stream_continues() {
        let (writer, mut lines) = SharedWriter::test_stub();
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

        let corrupt = r#"{"type":"stream_event""#;
        let mut counters = SkipCounters::default();
        let outcome =
            handle_line(&dispatch, &pending, &inflight, &events_tx, 7, corrupt, &mut counters)
                .await;

        assert_eq!(outcome, LineOutcome::Skipped, "a decode error must not end the read loop");
        assert!(events_rx.try_recv().is_err(), "a skipped line must not surface an event");
        assert!(
            lines.try_recv().is_err(),
            "a corrupt non-control line must not be answered on stdin"
        );

        let valid = r#"{"type":"stream_event","uuid":"evt-1","session_id":"sess-1","event":{"type":"message_start"}}"#;
        let not_a_request = r#"{"type":"rate_limit_event","request_id":"req_7"}"#;
        let outcome = handle_line(
            &dispatch,
            &pending,
            &inflight,
            &events_tx,
            8,
            not_a_request,
            &mut counters,
        )
        .await;
        assert_eq!(outcome, LineOutcome::Skipped);
        // The answer write is detached, so poll rather than try_recv.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), lines.recv())
                .await
                .is_err(),
            "only a corrupt control_request may be answered on stdin"
        );

        let outcome =
            handle_line(&dispatch, &pending, &inflight, &events_tx, 9, valid, &mut counters).await;

        assert_eq!(
            outcome,
            LineOutcome::Continue,
            "the read loop must still be alive after a skip"
        );
        match events_rx.try_recv() {
            Ok(Ok(Message::StreamEvent { uuid, .. })) => assert_eq!(uuid, "evt-1"),
            other => panic!("expected the next valid frame on the event stream, got {other:?}"),
        }
    }

    /// A control_request whose body fails to decode is answered with
    /// an error `control_response` before being skipped: the CLI
    /// blocks on an unanswered request, so silence would hang the
    /// turn even though the session survives.
    #[tokio::test]
    async fn a_malformed_control_request_is_answered_then_skipped() {
        let (writer, mut lines) = SharedWriter::test_stub();
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

        // `can_use_tool` without `tool_use_id` fails the custom
        // `ControlRequestKind` Deserialize, so the line is Malformed.
        let line = r#"{"type":"control_request","request_id":"req_9","request":{"subtype":"can_use_tool","tool_name":"Edit"}}"#;
        let mut counters = SkipCounters::default();
        let outcome =
            handle_line(&dispatch, &pending, &inflight, &events_tx, 7, line, &mut counters).await;

        assert_eq!(
            outcome,
            LineOutcome::Skipped,
            "a malformed control_request must not end the read loop"
        );
        assert!(events_rx.try_recv().is_err(), "nothing surfaces on the event stream");
        let written = tokio::time::timeout(std::time::Duration::from_secs(5), lines.recv())
            .await
            .expect("error control_response written to stdin within 5s")
            .expect("writer channel open");
        assert!(
            written.contains(r#""request_id":"req_9""#),
            "the response must name the request: {written}"
        );
        assert!(
            written.contains(r#""subtype":"error""#),
            "the response must be an error: {written}"
        );
        assert!(
            written.contains("tool_use_id"),
            "the error body must carry the real decode reason: {written}"
        );
    }

    /// A stream that is near-100% malformed presents to the user as a
    /// silent hang, so the reader escalates once to a session-visible
    /// error at the consecutive-skip threshold. A sparse bad frame
    /// never gets that far, and a good line resets the count.
    #[tokio::test]
    async fn a_degenerate_malformed_stream_escalates_once() {
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
        let mut counters = SkipCounters::default();
        let corrupt = r#"{"type":"stream_event""#;

        for line_no in 1..UNPARSABLE_ESCALATE_AFTER {
            let outcome = handle_line(
                &dispatch,
                &pending,
                &inflight,
                &events_tx,
                line_no,
                corrupt,
                &mut counters,
            )
            .await;
            assert_eq!(outcome, LineOutcome::Skipped);
            assert!(
                events_rx.try_recv().is_err(),
                "no escalation below the threshold (line {line_no})"
            );
        }

        let outcome = handle_line(
            &dispatch,
            &pending,
            &inflight,
            &events_tx,
            UNPARSABLE_ESCALATE_AFTER,
            corrupt,
            &mut counters,
        )
        .await;
        assert_eq!(outcome, LineOutcome::Skipped, "the escalation itself must not stop the loop");
        match events_rx.try_recv() {
            Ok(Err(_)) => {}
            other => panic!("expected the session-visible escalation error, got {other:?}"),
        }
        assert!(events_rx.try_recv().is_err(), "the escalation must fire exactly once");
    }

    #[tokio::test]
    async fn a_good_line_resets_the_consecutive_skip_count() {
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
        let mut counters = SkipCounters::default();
        let corrupt = r#"{"type":"stream_event""#;
        let valid = r#"{"type":"stream_event","uuid":"evt-1","session_id":"sess-1","event":{"type":"message_start"}}"#;

        let mut line_no = 0;
        for _ in 0..(UNPARSABLE_ESCALATE_AFTER - 1) {
            line_no += 1;
            handle_line(
                &dispatch,
                &pending,
                &inflight,
                &events_tx,
                line_no,
                corrupt,
                &mut counters,
            )
            .await;
        }
        line_no += 1;
        handle_line(&dispatch, &pending, &inflight, &events_tx, line_no, valid, &mut counters)
            .await;
        for _ in 0..(UNPARSABLE_ESCALATE_AFTER - 1) {
            line_no += 1;
            handle_line(
                &dispatch,
                &pending,
                &inflight,
                &events_tx,
                line_no,
                corrupt,
                &mut counters,
            )
            .await;
        }

        match events_rx.try_recv() {
            Ok(Ok(Message::StreamEvent { .. })) => {}
            other => panic!("expected the valid frame's event, got {other:?}"),
        }
        assert!(
            events_rx.try_recv().is_err(),
            "a good line between runs of corrupt lines must reset the escalation count"
        );
    }
}
