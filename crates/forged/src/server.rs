//! WebSocket server — bind, accept, dispatch.
//!
//! M1 ran the WS read and write through a single async task. M2 splits the
//! socket: `read_loop` parses inbound frames and pushes outbound work onto
//! a per-connection mpsc; `write_loop` drains the mpsc and serialises onto
//! the socket. The mpsc is the canonical write path so `session.event`
//! notifications, dispatched responses, and reverse-RPC requests (M4) can
//! all enter from any task without colliding on `WebSocketStream::send`.

use std::net::SocketAddr;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tracing::{debug, info, warn};

use crate::Error;
use crate::connection::{Connection, ConnectionId, Outbound};
use crate::jsonrpc::{Notification, Request, Response};
use crate::methods;
use crate::registry::DaemonState;

/// Accept loop. Spawns one task per accepted connection.
///
/// # Errors
///
/// Returns when the listener fails to accept (e.g. the listener is closed).
pub async fn run(listener: TcpListener, state: DaemonState) -> Result<(), Error> {
    info!(addr = ?listener.local_addr()?, "forged listening");
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, peer, state).await {
                        warn!(?peer, error = %e, "connection handler exited with error");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "accept failed");
                return Err(Error::Io(e));
            }
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    state: DaemonState,
) -> Result<(), Error> {
    let ws = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|e| Error::InternalError(format!("ws upgrade: {e}")))?;
    debug!(?peer, "ws upgraded");

    let (write, read) = ws.split();
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Outbound>();
    let conn = Connection::new(ConnectionId::new(), out_tx.clone());
    state.register_connection(conn.clone());

    // Spawn the writer task — drains out_rx, sends to WS.
    let writer = tokio::spawn(write_loop(write, out_rx));

    // Send the initial client.identify notification through the same channel
    // so it interleaves correctly with later traffic.
    let _ = out_tx.send(Outbound::Notification(Notification::new(
        "client.identify",
        serde_json::json!({
            "connection_id": conn.id.0,
            "server_version": env!("CARGO_PKG_VERSION"),
            "server_build": option_env!("FORGED_BUILD_SHA").unwrap_or("dev"),
        }),
    )));

    let r = read_loop(read, &conn, &state).await;

    state.unregister_connection(&conn.id);
    // Drop our end of the channel so the writer task observes EOF and exits.
    drop(out_tx);
    let _ = writer.await;
    r
}

async fn write_loop(
    mut write: SplitSink<WebSocketStream<TcpStream>, WsMsg>,
    mut rx: mpsc::UnboundedReceiver<Outbound>,
) -> Result<(), Error> {
    while let Some(frame) = rx.recv().await {
        let text = match frame.to_text() {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "outbound encode failed; dropping frame");
                continue;
            }
        };
        if let Err(e) = write.send(WsMsg::Text(text)).await {
            warn!(error = %e, "write loop send failed; closing");
            break;
        }
    }
    let _ = write.close().await;
    Ok(())
}

async fn read_loop(
    mut read: SplitStream<WebSocketStream<TcpStream>>,
    conn: &Connection,
    state: &DaemonState,
) -> Result<(), Error> {
    while let Some(msg) = read.next().await {
        let msg = msg.map_err(|e| Error::InternalError(format!("ws recv: {e}")))?;
        let text = match msg {
            WsMsg::Text(t) => t,
            WsMsg::Close(_) => break,
            WsMsg::Ping(_) | WsMsg::Pong(_) => continue,
            other => {
                warn!(?other, "ignoring non-text ws frame");
                continue;
            }
        };

        // Peek at the JSON shape: a frame with no `method`, an `id`,
        // and (`result` | `error`) is an inbound JSON-RPC response. If
        // the id starts `rev_`, it's a reply to a daemon-issued
        // reverse-RPC (M4) — route it to the resolver and continue
        // without trying to parse as a Request.
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let _ = conn.outbound.send(Outbound::Response(Response::error(
                    Value::Null,
                    Error::ParseError(e.to_string()).to_jsonrpc(),
                )));
                continue;
            }
        };

        let is_response = v.get("method").is_none()
            && v.get("id").is_some()
            && (v.get("result").is_some() || v.get("error").is_some());
        if is_response {
            let id = v.get("id").and_then(Value::as_str).unwrap_or("");
            if id.starts_with("rev_") {
                // Use `result` when present; on error responses pass an
                // object containing the error payload so the SDK
                // callback can decide how to handle it (currently
                // permissive — wraps the error as the answer).
                let value = v
                    .get("result")
                    .cloned()
                    .or_else(|| v.get("error").cloned())
                    .unwrap_or(Value::Null);
                crate::reverse_rpc::resolve(state, id, value);
            }
            // Either we just resolved a reverse-RPC, or the id didn't
            // match anything outstanding — both cases are silent.
            continue;
        }

        let req: Request = match serde_json::from_value(v) {
            Ok(r) => r,
            Err(e) => {
                let _ = conn.outbound.send(Outbound::Response(Response::error(
                    Value::Null,
                    Error::ParseError(e.to_string()).to_jsonrpc(),
                )));
                continue;
            }
        };

        let resp = dispatch(&req, conn, state).await;
        let _ = conn.outbound.send(Outbound::Response(resp));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn dispatch(req: &Request, conn: &Connection, state: &DaemonState) -> Response {
    let id = req.id.clone();
    let result: Result<Value, Error> = match req.method.as_str() {
        // ---- daemon.* -------------------------------------------------------
        "daemon.status" => methods::daemon::status(state)
            .await
            .and_then(|s| serde_json::to_value(s).map_err(Error::Json)),
        // ---- session.* ------------------------------------------------------
        "session.spawn" => {
            let raw = req
                .params
                .clone()
                .unwrap_or_else(|| Value::Object(serde_json::Map::default()));
            match methods::session::parse_spawn_params(&raw) {
                Ok(params) => methods::session::spawn(state, params)
                    .await
                    .and_then(|r| serde_json::to_value(r).map_err(Error::Json)),
                Err(e) => Err(e),
            }
        }
        "session.send_user_message" => {
            match parse_params::<methods::session::SendUserMessageParams>(req.params.as_ref()) {
                Ok(p) => methods::session::send_user_message(state, &p.session_id, &p.prompt)
                    .await
                    .map(|()| Value::Null),
                Err(e) => Err(e),
            }
        }
        "session.subscribe" => {
            match parse_params::<methods::session::SubscribeParams>(req.params.as_ref()) {
                Ok(p) => methods::session::subscribe(state, conn, &p.session_id, p.since)
                    .and_then(|r| serde_json::to_value(r).map_err(Error::Json)),
                Err(e) => Err(e),
            }
        }
        "session.unsubscribe" => {
            match parse_params::<methods::session::UnsubscribeParams>(req.params.as_ref()) {
                Ok(p) => {
                    methods::session::unsubscribe(state, conn, &p.session_id).map(|()| Value::Null)
                }
                Err(e) => Err(e),
            }
        }
        "session.disconnect" => {
            match parse_params::<methods::session::DisconnectParams>(req.params.as_ref()) {
                Ok(p) => methods::session::disconnect(state, &p.session_id)
                    .await
                    .map(|()| Value::Null),
                Err(e) => Err(e),
            }
        }
        "session.end_input" => {
            match parse_params::<methods::session::EndInputParams>(req.params.as_ref()) {
                Ok(p) => methods::session::end_input(state, &p.session_id)
                    .await
                    .map(|()| Value::Null),
                Err(e) => Err(e),
            }
        }
        "session.interrupt" => match parse_params::<SessionIdOnlyParams>(req.params.as_ref()) {
            Ok(p) => methods::session::interrupt(state, &p.session_id)
                .await
                .map(|()| Value::Null),
            Err(e) => Err(e),
        },
        "session.set_permission_mode" => {
            match parse_params::<SetPermissionModeParams>(req.params.as_ref()) {
                Ok(p) => match parse_permission_mode(&p.mode) {
                    Ok(mode) => methods::session::set_permission_mode(state, &p.session_id, mode)
                        .await
                        .map(|()| Value::Null),
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            }
        }
        "session.set_model" => match parse_params::<SetModelParams>(req.params.as_ref()) {
            Ok(p) => methods::session::set_model(state, &p.session_id, p.model)
                .await
                .map(|()| Value::Null),
            Err(e) => Err(e),
        },
        "session.rewind_files" => match parse_params::<RewindFilesParams>(req.params.as_ref()) {
            Ok(p) => methods::session::rewind_files(state, &p.session_id, p.user_message_id)
                .await
                .map(|()| Value::Null),
            Err(e) => Err(e),
        },
        "session.stop_task" => match parse_params::<StopTaskParams>(req.params.as_ref()) {
            Ok(p) => methods::session::stop_task(state, &p.session_id, p.task_id)
                .await
                .map(|()| Value::Null),
            Err(e) => Err(e),
        },
        // ---- sessions.* (filesystem) ----------------------------------------
        "sessions.list" => match parse_params::<SessionsListParams>(req.params.as_ref()) {
            Ok(p) => methods::sessions::list(p.directory, p.limit, p.offset).and_then(|v| {
                serde_json::to_value(serde_json::json!({ "sessions": v })).map_err(Error::Json)
            }),
            Err(e) => Err(e),
        },
        "sessions.info" => match parse_params::<SessionsInfoParams>(req.params.as_ref()) {
            Ok(p) => methods::sessions::info(p.session_id, p.directory)
                .and_then(|v| serde_json::to_value(v).map_err(Error::Json)),
            Err(e) => Err(e),
        },
        "sessions.messages" => match parse_params::<SessionsInfoParams>(req.params.as_ref()) {
            Ok(p) => methods::sessions::messages(p.session_id, p.directory)
                .and_then(|v| serde_json::to_value(v).map_err(Error::Json)),
            Err(e) => Err(e),
        },
        "sessions.list_subagents" => {
            match parse_params::<SessionsInfoParams>(req.params.as_ref()) {
                Ok(p) => {
                    methods::sessions::list_subagents(p.session_id, p.directory).and_then(|v| {
                        serde_json::to_value(serde_json::json!({ "subagent_ids": v }))
                            .map_err(Error::Json)
                    })
                }
                Err(e) => Err(e),
            }
        }
        "sessions.subagent_messages" => {
            match parse_params::<SessionsSubagentMessagesParams>(req.params.as_ref()) {
                Ok(p) => {
                    methods::sessions::subagent_messages(p.session_id, p.subagent_id, p.directory)
                        .and_then(|v| {
                            serde_json::to_value(serde_json::json!({ "messages": v }))
                                .map_err(Error::Json)
                        })
                }
                Err(e) => Err(e),
            }
        }
        "sessions.project_key" => {
            match parse_params::<SessionsProjectKeyParams>(req.params.as_ref()) {
                Ok(p) => methods::sessions::project_key(p.path).and_then(|v| {
                    serde_json::to_value(serde_json::json!({ "project_key": v }))
                        .map_err(Error::Json)
                }),
                Err(e) => Err(e),
            }
        }
        "sessions.rename" => match parse_params::<SessionsRenameParams>(req.params.as_ref()) {
            Ok(p) => {
                methods::sessions::rename(p.session_id, p.title, p.directory).map(|()| Value::Null)
            }
            Err(e) => Err(e),
        },
        "sessions.tag" => match parse_params::<SessionsTagParams>(req.params.as_ref()) {
            Ok(p) => methods::sessions::tag(p.session_id, p.tag, p.directory).map(|()| Value::Null),
            Err(e) => Err(e),
        },
        "sessions.delete" => match parse_params::<SessionsInfoParams>(req.params.as_ref()) {
            Ok(p) => methods::sessions::delete(p.session_id, p.directory).map(|()| Value::Null),
            Err(e) => Err(e),
        },
        "sessions.fork" => match parse_params::<SessionsForkParams>(req.params.as_ref()) {
            Ok(p) => {
                methods::sessions::fork(p.session_id, p.up_to_message_id, p.title, p.directory)
                    .and_then(|v| serde_json::to_value(v).map_err(Error::Json))
            }
            Err(e) => Err(e),
        },
        // ---- mcp.* ---------------------------------------------------------
        "mcp.status" => match parse_params::<SessionIdOnlyParams>(req.params.as_ref()) {
            Ok(p) => methods::mcp::status(state, &p.session_id)
                .await
                .and_then(|r| serde_json::to_value(r).map_err(Error::Json)),
            Err(e) => Err(e),
        },
        "mcp.reconnect" => match parse_params::<McpReconnectParams>(req.params.as_ref()) {
            Ok(p) => methods::mcp::reconnect(state, &p.session_id, &p.server_name)
                .await
                .map(|()| Value::Null),
            Err(e) => Err(e),
        },
        "mcp.toggle" => match parse_params::<McpToggleParams>(req.params.as_ref()) {
            Ok(p) => methods::mcp::toggle(state, &p.session_id, &p.server_name, p.enabled)
                .await
                .map(|()| Value::Null),
            Err(e) => Err(e),
        },
        // ---- context.* -----------------------------------------------------
        "context.get" => match parse_params::<SessionIdOnlyParams>(req.params.as_ref()) {
            Ok(p) => methods::context::get(state, &p.session_id)
                .await
                .and_then(|r| serde_json::to_value(r).map_err(Error::Json)),
            Err(e) => Err(e),
        },
        // ---- prompts.* (M4) ------------------------------------------------
        "prompts.respond" => match parse_params::<PromptsRespondParams>(req.params.as_ref()) {
            Ok(p) => methods::prompts::respond(state, &p.session_id, &p.prompt_id, p.result)
                .map(|()| Value::Null),
            Err(e) => Err(e),
        },
        other => Err(Error::MethodNotFound(other.to_string())),
    };
    match result {
        Ok(v) => Response::success(id, v),
        Err(e) => Response::error(id, e.to_jsonrpc()),
    }
}

/// Deserialize `params` into a typed shape, mapping serde failures to
/// [`Error::InvalidParams`]. Treats absent / null params as the empty
/// object so handlers can decide for themselves whether their fields are
/// optional.
fn parse_params<T: for<'de> serde::Deserialize<'de>>(params: Option<&Value>) -> Result<T, Error> {
    let raw = params
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::default()));
    serde_json::from_value(raw).map_err(|e| Error::InvalidParams(e.to_string()))
}

/// Translate a wire-shape `permission_mode` string into the SDK enum.
fn parse_permission_mode(s: &str) -> Result<forge_sdk::PermissionMode, Error> {
    match s {
        "ask" => Ok(forge_sdk::PermissionMode::Ask),
        "accept_edits" => Ok(forge_sdk::PermissionMode::AcceptEdits),
        "plan" => Ok(forge_sdk::PermissionMode::Plan),
        "bypass_permissions" => Ok(forge_sdk::PermissionMode::BypassPermissions),
        "auto" => Ok(forge_sdk::PermissionMode::Auto),
        "deny_permissions" => Ok(forge_sdk::PermissionMode::DenyPermissions),
        other => Err(Error::InvalidParams(format!(
            "permission_mode: unknown variant '{other}'"
        ))),
    }
}

// ---- Param shapes scoped to dispatch -----------------------------------

#[derive(serde::Deserialize)]
struct SessionIdOnlyParams {
    session_id: crate::session_state::SessionId,
}

#[derive(serde::Deserialize)]
struct SetPermissionModeParams {
    session_id: crate::session_state::SessionId,
    mode: String,
}

#[derive(serde::Deserialize)]
struct SetModelParams {
    session_id: crate::session_state::SessionId,
    #[serde(default)]
    model: Option<String>,
}

#[derive(serde::Deserialize)]
struct RewindFilesParams {
    session_id: crate::session_state::SessionId,
    user_message_id: String,
}

#[derive(serde::Deserialize)]
struct StopTaskParams {
    session_id: crate::session_state::SessionId,
    task_id: String,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct SessionsListParams {
    directory: Option<String>,
    limit: Option<usize>,
    offset: usize,
}

#[derive(serde::Deserialize)]
struct SessionsInfoParams {
    session_id: String,
    #[serde(default)]
    directory: Option<String>,
}

#[derive(serde::Deserialize)]
struct SessionsSubagentMessagesParams {
    session_id: String,
    subagent_id: String,
    #[serde(default)]
    directory: Option<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct SessionsProjectKeyParams {
    path: Option<String>,
}

#[derive(serde::Deserialize)]
struct SessionsRenameParams {
    session_id: String,
    title: String,
    #[serde(default)]
    directory: Option<String>,
}

#[derive(serde::Deserialize)]
struct SessionsTagParams {
    session_id: String,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    directory: Option<String>,
}

#[derive(serde::Deserialize)]
struct SessionsForkParams {
    session_id: String,
    #[serde(default)]
    up_to_message_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    directory: Option<String>,
}

#[derive(serde::Deserialize)]
struct McpReconnectParams {
    session_id: crate::session_state::SessionId,
    server_name: String,
}

#[derive(serde::Deserialize)]
struct McpToggleParams {
    session_id: crate::session_state::SessionId,
    server_name: String,
    enabled: bool,
}

#[derive(serde::Deserialize)]
struct PromptsRespondParams {
    session_id: crate::session_state::SessionId,
    prompt_id: String,
    result: Value,
}
