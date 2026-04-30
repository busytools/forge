//! Session actor task — owns the [`forge_sdk::Client`] for the lifetime
//! of one session and drives it via a `tokio::select!` loop over the
//! command channel and [`forge_sdk::Client::next_event`].
//!
//! Extracted from `methods/session.rs` (audit 2026-04-26 god-file
//! split). The handler proxies in `methods::session` enqueue
//! [`Command`]s on the session's mpsc; the actor dequeues and runs
//! them on `&Client` (the SDK's command methods all take `&self`
//! against an `Arc`-backed handle). Outbound message events fan out
//! to subscribers as `session.event` notifications.
//!
//! ## Control-request dispatch
//!
//! Inbound `control_request`s (hook callbacks, MCP messages,
//! `can_use_tool`) are dispatched **inside the SDK** on
//! `tokio::spawn`'d tasks — see
//! [`forge_sdk::Client::next_event`] for the detachment story. That
//! closes the audit 2026-04-26 G1 hazard (the actor's `select!`
//! cancellation no longer drops a `control_response` write
//! mid-flight) and means the actor itself only ever observes
//! [`forge_sdk::Message`]s.

use forge_sdk::Client;

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
    client: Client,
    mut commands: tokio::sync::mpsc::Receiver<Command>,
) {
    let session_id = handle.id.clone();
    tokio::spawn(async move {
        // Track the current model id. Seeded from the captured
        // `system/init` payload, updated on every `Command::SetModel`.
        let mut current_model: Option<String> = client
            .initial_session_data()
            .and_then(|d| d.get("model"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
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
                            if r.is_ok() {
                                current_model = model.clone();
                            }
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
                        Command::CurrentModel { reply } => {
                            let _ = reply.send(Ok(current_model.clone()));
                        }
                        Command::StatusSnapshot { reply } => {
                            let account = client
                                .initial_session_data()
                                .and_then(|d| d.get("account"))
                                .map(|v| {
                                    let s = |k: &str| {
                                        v.get(k).and_then(|x| x.as_str()).map(str::to_owned)
                                    };
                                    crate::methods::session::AccountSnapshot {
                                        email: s("email"),
                                        organization: s("organization"),
                                        subscription_type: s("subscriptionType"),
                                        token_source: s("tokenSource"),
                                        api_key_source: s("apiKeySource"),
                                        api_provider: s("apiProvider"),
                                    }
                                })
                                .unwrap_or_default();
                            let _ = reply.send(Ok(account));
                        }
                        Command::GenerateSessionTitle { description, reply } => {
                            let r = client
                                .generate_session_title(&description)
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::PluginsReload { reply } => {
                            let r = client.reload_plugins().await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::McpSetServers { servers, reply } => {
                            let r = client.mcp_set_servers(servers).await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::McpAuthenticate { server_name, reply } => {
                            let r = client
                                .mcp_authenticate(&server_name)
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::McpClearAuth { server_name, reply } => {
                            let r =
                                client.mcp_clear_auth(&server_name).await.map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::McpOauthCallback {
                            server_name,
                            callback_url,
                            reply,
                        } => {
                            let r = client
                                .mcp_oauth_callback_url(&server_name, &callback_url)
                                .await
                                .map_err(Error::Sdk);
                            let _ = reply.send(r);
                        }
                        Command::SlashList { reply } => {
                            let commands = client
                                .initial_session_data()
                                .and_then(|d| d.get("slash_commands"))
                                .and_then(serde_json::Value::as_array)
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|cmd| {
                                            let name =
                                                cmd.get("name").and_then(|v| v.as_str())?;
                                            let description = cmd
                                                .get("description")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("");
                                            Some((name.to_owned(), description.to_owned()))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let _ = reply.send(Ok(commands));
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
