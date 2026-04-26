//! WebSocket server — bind, accept, dispatch.
//!
//! M1 ran the WS read and write through a single async task. M2 splits the
//! socket: `read_loop` parses inbound frames and pushes outbound work onto
//! a per-connection mpsc; `write_loop` drains the mpsc and serialises onto
//! the socket. The mpsc is the canonical write path so `session.event`
//! notifications, dispatched responses, and reverse-RPC requests (M4) can
//! all enter from any task without colliding on `WebSocketStream::send`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::handshake::server::{Request as TungReq, Response as TungResp};
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
    // Capture the request URI's query string from inside the handshake
    // callback so we can extract the friendly `?name=<value>` the client
    // supplied. The callback runs synchronously inside the handshake; we
    // stash the parsed value in an Arc<Mutex> the outer task reads after
    // the upgrade completes.
    let captured_name: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cap_for_callback = captured_name.clone();
    // tungstenite's handshake callback returns `Result<Response, ErrorResponse>`
    // where `ErrorResponse` (an `http::Response<Option<String>>`) is much
    // larger than the success variant. We never return Err from the
    // callback (any handshake-level rejection is reserved for future
    // auth work), so the size delta isn't a concern here.
    #[allow(
        clippy::result_large_err,
        reason = "tungstenite's handshake-callback Err variant is large; we never return Err from this callback so the size delta is dead-code"
    )]
    let ws = tokio_tungstenite::accept_hdr_async(stream, move |req: &TungReq, resp: TungResp| {
        if let Some(query) = req.uri().query() {
            for (k, v) in parse_query(query) {
                if k == "name" && !v.is_empty() {
                    *cap_for_callback.lock() = Some(v);
                }
            }
        }
        Ok(resp)
    })
    .await
    .map_err(|e| Error::InternalError(format!("ws upgrade: {e}")))?;
    debug!(?peer, "ws upgraded");

    let name = captured_name.lock().clone();

    let (write, read) = ws.split();
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Outbound>();
    let conn =
        Connection::with_metadata(ConnectionId::new(), name, SystemTime::now(), out_tx.clone());
    state.register_connection(conn.clone());

    // Spawn the writer task — drains out_rx, sends to WS.
    let writer = tokio::spawn(write_loop(write, out_rx));

    // Send the initial client.identify notification through the same channel
    // so it interleaves correctly with later traffic.
    if out_tx
        .send(Outbound::Notification(Notification::new(
            "client.identify",
            serde_json::json!({
                "connection_id": conn.id.0,
                "server_version": env!("CARGO_PKG_VERSION"),
                "server_build": option_env!("FORGED_BUILD_SHA").unwrap_or("dev"),
            }),
        )))
        .is_err()
    {
        // Round 4 — fix M2. The writer task drains `out_rx` and only
        // closes when its end of the channel is dropped — at handshake
        // time we haven't dropped it yet, so reaching this branch means
        // the writer task already exited (panic, transport already
        // dead). Surface it so a borked handshake leaves a trail in
        // operator logs; the read loop will also exit shortly when the
        // socket signals closed.
        tracing::warn!(
            conn_id = %conn.id.0,
            "client.identify send failed at handshake (writer task gone)"
        );
    }

    let r = read_loop(read, &conn, &state).await;

    let cleared_sessions = state.unregister_connection(&conn.id);
    // For each session whose primary was cleared, broadcast
    // `session.primary_changed { primary: null, reason: "disconnected" }`
    // so subscribers (viewers + reconnected primary candidates) know.
    for sid in cleared_sessions {
        let frame = Outbound::Notification(Notification::new(
            "session.primary_changed",
            serde_json::json!({
                "session_id": sid.0,
                "primary": serde_json::Value::Null,
                "previous": conn.id.0,
                "reason": "disconnected",
            }),
        ));
        crate::broadcast::fanout(&state, &sid, &frame);
    }
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
                // Round 5 — symmetry trace. The writer task drains
                // `conn.outbound`; reaching the err branch means it
                // already exited (panic, transport closed). Benign —
                // the read loop will exit shortly when the socket
                // signals closed — but tracing makes late drops
                // attributable during incident response.
                if conn
                    .outbound
                    .send(Outbound::Response(Response::error(
                        Value::Null,
                        Error::ParseError(e.to_string()).to_jsonrpc(),
                    )))
                    .is_err()
                {
                    tracing::trace!(
                        conn_id = %conn.id.0,
                        "server: parse-error response send dropped (writer task gone)"
                    );
                }
                continue;
            }
        };

        let is_response = v.get("method").is_none()
            && v.get("id").is_some()
            && (v.get("result").is_some() || v.get("error").is_some());
        if is_response {
            // JSON-RPC 2.0 §4.2 allows id to be string OR number. The
            // daemon's reverse-RPC issuer always uses string `rev_<uuid>`,
            // but a strict client mirroring the inbound id type might
            // reply with a number. Stringify for lookup; null / bool /
            // composite ids are genuinely malformed.
            let id_owned = match v.get("id") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                other => {
                    tracing::warn!(
                        id = ?other,
                        "received malformed id in JSON-RPC response (expected string or number); ignoring"
                    );
                    continue;
                }
            };
            let id = id_owned.as_str();
            if id.starts_with("rev_") {
                // Distinguish success from error: on `result` resolve
                // normally; on `error` route through `resolve_error`
                // so the bridges can map to a typed deny with reason
                // rather than collapsing both cases into a generic
                // failure.
                if let Some(result) = v.get("result").cloned() {
                    crate::reverse_rpc::resolve(state, id, result);
                } else if let Some(err) = v.get("error") {
                    crate::reverse_rpc::resolve_error(state, id, err);
                } else {
                    crate::reverse_rpc::resolve(state, id, Value::Null);
                }
            } else {
                // Round 4 — fix M1. Inbound JSON-RPC response with a
                // non-`rev_` id reaches this branch only when a client
                // forges a reply for an id the daemon never issued
                // (the daemon's own outbound requests all use `rev_`
                // prefixes). Trace the drop so misbehaving clients
                // are visible in operator logs rather than silently
                // dropped on the floor.
                tracing::trace!(id, "received response for non-rev id; ignoring");
            }
            continue;
        }

        let req: Request = match serde_json::from_value(v) {
            Ok(r) => r,
            Err(e) => {
                // Round 5 — symmetry trace. Same writer-task drain
                // path as above; surface invalid-request response
                // drops so they're attributable.
                if conn
                    .outbound
                    .send(Outbound::Response(Response::error(
                        Value::Null,
                        Error::ParseError(e.to_string()).to_jsonrpc(),
                    )))
                    .is_err()
                {
                    tracing::trace!(
                        conn_id = %conn.id.0,
                        "server: invalid-request response send dropped (writer task gone)"
                    );
                }
                continue;
            }
        };

        let resp = dispatch(&req, conn, state).await;
        // Round 5 — symmetry trace. Dispatched response (success or
        // error envelope) reaches a closed `outbound` only when the
        // writer task already exited; surface drops so they're
        // attributable in operator logs.
        if conn.outbound.send(Outbound::Response(resp)).is_err() {
            tracing::trace!(
                conn_id = %conn.id.0,
                "server: dispatch response send dropped (writer task gone)"
            );
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one match arm per supported method by design; splitting would obscure the dispatch table"
)]
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
        // ---- session.* multi-client (M5) -----------------------------------
        "session.claim_primary" => match parse_params::<SessionIdOnlyParams>(req.params.as_ref()) {
            Ok(p) => methods::multi_client::claim_primary(state, &conn.id, &p.session_id)
                .map(|()| Value::Null),
            Err(e) => Err(e),
        },
        "session.peers" => match parse_params::<SessionIdOnlyParams>(req.params.as_ref()) {
            Ok(p) => methods::multi_client::peers(state, &p.session_id)
                .and_then(|r| serde_json::to_value(r).map_err(Error::Json)),
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

use crate::session_state::parse_permission_mode;

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

// =============================================================================
// Handshake query-string parsing — used by `handle_connection` to extract
// the friendly `?name=<value>` the client supplies. Hand-rolled to avoid
// pulling in the `url` crate just for one read site.
// =============================================================================

/// Iterate over `key=value` pairs in a query string. Uses
/// percent-decoding via [`url_decode`] on each side. Empty trailing
/// segments and keys without `=` map to the empty string for `value`.
fn parse_query(q: &str) -> impl Iterator<Item = (String, String)> + '_ {
    q.split('&').filter_map(|pair| {
        if pair.is_empty() {
            return None;
        }
        let mut it = pair.splitn(2, '=');
        let k_raw = it.next()?;
        let v_raw = it.next().unwrap_or("");
        Some((url_decode(k_raw), url_decode(v_raw)))
    })
}

/// Tiny `application/x-www-form-urlencoded` decoder. Handles `+` →
/// space and `%XX` hex escapes. Decodes byte-by-byte and runs
/// `String::from_utf8_lossy` on the buffer at the end so multi-byte
/// UTF-8 sequences (e.g. `%C3%A9` → "é") round-trip correctly. ASCII
/// characters are preserved; malformed UTF-8 falls back to U+FFFD.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex_slice = bytes.get(i + 1..i + 3);
                let hex_str = hex_slice.and_then(|b| std::str::from_utf8(b).ok());
                if let Some(h) = hex_str {
                    if let Ok(n) = u8::from_str_radix(h, 16) {
                        out.push(n);
                        i += 3;
                        continue;
                    }
                }
                // Malformed escape — treat the `%` as a literal byte and
                // surface a debug log so operators can trace bad query
                // strings rather than seeing them silently mangled.
                tracing::debug!(
                    bytes = ?hex_slice,
                    "url_decode: malformed percent escape, passing literal"
                );
                out.push(b'%');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{parse_query, url_decode};

    #[test]
    fn parse_query_extracts_simple_pair() {
        let parsed: Vec<(String, String)> = parse_query("name=studio-terminal").collect();
        assert_eq!(parsed, vec![("name".into(), "studio-terminal".into())]);
    }

    #[test]
    fn parse_query_decodes_percent_escapes_in_value() {
        let parsed: Vec<(String, String)> = parse_query("name=hello%20world").collect();
        assert_eq!(parsed, vec![("name".into(), "hello world".into())]);
    }

    #[test]
    fn parse_query_handles_multiple_pairs_and_skips_empty_segments() {
        let parsed: Vec<(String, String)> = parse_query("a=1&b=2&&c=3").collect();
        assert_eq!(
            parsed,
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "2".into()),
                ("c".into(), "3".into()),
            ]
        );
    }

    #[test]
    fn url_decode_handles_plus_as_space() {
        assert_eq!(url_decode("foo+bar"), "foo bar");
    }

    #[test]
    fn url_decode_decodes_two_byte_utf8_e_acute() {
        // %C3%A9 — UTF-8 for "é"
        assert_eq!(url_decode("caf%C3%A9"), "café");
    }

    #[test]
    fn url_decode_decodes_three_byte_utf8_check_mark() {
        // %E2%9C%93 — UTF-8 for "✓"
        assert_eq!(url_decode("ok%20%E2%9C%93"), "ok ✓");
    }

    #[test]
    fn url_decode_substitutes_replacement_char_on_truncated_utf8() {
        // %C3 by itself is the start of a 2-byte sequence with no
        // continuation byte — String::from_utf8_lossy substitutes
        // U+FFFD.
        let decoded = url_decode("bad%C3");
        assert!(
            decoded.contains('\u{FFFD}'),
            "expected U+FFFD on truncated UTF-8, got {decoded:?}"
        );
    }
}
