//! Session actor task — owns the [`forge_sdk::Client`] for the lifetime
//! of one session and drives it via a `tokio::select!` loop over the
//! command channel and `next_event_returning_control`.
//!
//! Extracted from `methods/session.rs` (audit 2026-04-26 god-file
//! split). The handler proxies in `methods::session` enqueue
//! [`Command`]s on the session's mpsc; the actor dequeues and runs
//! them on `&mut Client`. Outbound message events fan out to
//! subscribers as `session.event` notifications.
//!
//! ## Control-request dispatch
//!
//! Inbound `control_request`s (hook callbacks, MCP messages,
//! `can_use_tool`) are NOT dispatched on the actor. The actor uses
//! [`forge_sdk::Client::next_event_returning_control`] which returns
//! the request to the actor; the actor then `tokio::spawn`s
//! [`forge_sdk::ControlDispatchHandle::dispatch`] on a separate task
//! that holds an [`forge_sdk::AsyncWriter`] clone of the
//! [`crate::bridged_transport::BridgedTransport`] writer.
//!
//! That detachment closes the audit 2026-04-26 G1 hazard: a `Command`
//! preempting `next_event` mid-callback no longer cancels the
//! callback's eventual `control_response` write — the spawned task
//! runs to completion regardless of the actor's `select!`
//! cancellation.

use forge_sdk::{Client, ControlDispatchHandle, EventOrControl};

use crate::Error;
use crate::registry::DaemonState;
use crate::session_state::{Command, SessionHandle};

/// Extract a stable per-message identifier when one exists. Used as the
/// `event_id` field on `session.event` notifications.
///
/// `forge_sdk::Message` carries a `uuid` field on most variants but not all
/// — `Error`, `Unknown`, `System`, plus `Assistant`/`User`/`Result` when
/// the CLI hasn't been configured to emit them. Callers should treat the
/// empty string as "no id".
#[must_use]
pub(crate) fn message_event_id(msg: &forge_sdk::Message) -> &str {
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
        _ => {
            // Round 4 — fix m1. Trace the variant we couldn't extract
            // a UUID from so future SDK variant additions are
            // discoverable in operator traces rather than silently
            // emitting empty `event_id` strings.
            tracing::trace!(
                ?msg,
                "message_event_id: unrecognised variant; emitting empty"
            );
            ""
        }
    }
}

/// Spawn the actor task that exclusively owns `client` for the lifetime
/// of the session.
///
/// The actor `select!`s between:
///
/// 1. Inbound [`Command`]s from dispatch handlers — runs them on the
///    [`Client`].
/// 2. Outbound `next_event` calls — fans each [`forge_sdk::Message`]
///    to the session's subscribers as a `session.event` notification.
#[allow(
    clippy::too_many_lines,
    reason = "one match arm per Command variant by design; the actor's command dispatch table is the natural shape"
)]
pub(crate) fn spawn_session_actor(
    state: DaemonState,
    handle: &SessionHandle,
    mut client: Client,
    mut commands: tokio::sync::mpsc::Receiver<Command>,
) {
    let session_id = handle.id.clone();
    // Pull the dispatch handle once — it's cheap-clonable. Each
    // inbound control_request gets its own clone moved into a
    // `tokio::spawn`'d task so the write completes regardless of
    // the actor's `select!` cancellation. Closes audit 2026-04-26 G1.
    //
    // BridgedTransport returns `Some` here; if we ever swap to a
    // transport that returns `None` from `try_clone_writer` the actor
    // would panic, which is the right blast radius — silently
    // falling back to inline dispatch would re-introduce the cancel
    // hazard.
    let Some(dispatch_handle): Option<ControlDispatchHandle> = client.try_dispatch_handle() else {
        // BridgedTransport always returns Some from try_clone_writer.
        // Reaching None means a future swap to a single-task transport;
        // refuse to start rather than silently re-introduce the
        // cancel-mid-callback hazard.
        tracing::error!(
            session_id = %handle.id.0,
            "session actor: transport does not support try_clone_writer; refusing to spawn"
        );
        return;
    };
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
                        Command::Interrupt { reply } => {
                            let r = client.interrupt().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::SetPermissionMode { mode, reply } => {
                            let r = client.set_permission_mode(mode).await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::SetModel { model, reply } => {
                            let r = client
                                .set_model(model.as_deref())
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::RewindFiles { user_message_id, reply } => {
                            let r = client
                                .rewind_files(&user_message_id)
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::StopTask { task_id, reply } => {
                            let r = client.stop_task(&task_id).await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::McpStatus { reply } => {
                            let r = client.mcp_status().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::McpReconnect { server_name, reply } => {
                            let r = client
                                .mcp_reconnect(&server_name)
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::McpToggle { server_name, enabled, reply } => {
                            let r = client
                                .mcp_toggle(&server_name, enabled)
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::ContextGet { reply } => {
                            let r = client.get_context_usage().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                    }
                }
                next = client.next_event_returning_control() => {
                    match next {
                        Ok(Some(EventOrControl::Control(req))) => {
                            // Detached dispatch: clone the handle, move
                            // into a spawn'd task. The task writes its
                            // control_response via the cloned writer
                            // regardless of whether the actor's
                            // select! cancels this branch on the next
                            // iteration. Errors are logged; the
                            // session continues — a control_request
                            // failure is per-request, not session-level.
                            let handle = dispatch_handle.clone();
                            let sid = session_id.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle.dispatch(req).await {
                                    tracing::warn!(
                                        session_id = %sid.0,
                                        error = %e,
                                        "control_dispatch failed",
                                    );
                                }
                            });
                            // Keep listening — fall through to the next
                            // select! iteration without breaking.
                        }
                        Ok(Some(EventOrControl::Message(msg))) => {
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
                        Ok(Some(_)) => {
                            // Forward-compat: EventOrControl is
                            // non_exhaustive. Future SDK variants
                            // (e.g. a separate ControlCancel surface)
                            // log + skip rather than panic.
                            tracing::debug!(
                                session_id = %session_id.0,
                                "next_event_returning_control: unhandled variant",
                            );
                        }
                        Err(e) => {
                            tracing::warn!(session_id = %session_id.0, error = %e, "next_event error");
                            break "error";
                        }
                    }
                }
            }
        };

        // Drain any parked prompts before unregistering — otherwise
        // SDK callbacks awaiting on parked oneshots wait the full 1h
        // timeout. Each parked prompt gets a synthetic
        // `_session_closed: true` answer so the bridge unblocks
        // immediately, plus a `prompts.expired` broadcast so any
        // subscribers know the prompt is gone.
        crate::reverse_rpc::drain_prompts_on_session_exit(&state, &session_id);

        // Emit session.closed to all subscribers.
        let closed = crate::connection::Outbound::Notification(crate::jsonrpc::Notification::new(
            "session.closed",
            serde_json::json!({
                "session_id": session_id.0,
                "reason": reason,
            }),
        ));
        crate::broadcast::fanout(&state, &session_id, &closed);

        // Unregister the session — frees state.
        state.unregister_session(&session_id);
    });
}
