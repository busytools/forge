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

        let req: Request = match serde_json::from_str(&text) {
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

async fn dispatch(req: &Request, conn: &Connection, state: &DaemonState) -> Response {
    let id = req.id.clone();
    let result: Result<Value, Error> = match req.method.as_str() {
        "daemon.status" => methods::daemon::status(state)
            .await
            .and_then(|s| serde_json::to_value(s).map_err(Error::Json)),
        "session.spawn" => {
            let opts = parse_spawn_params(req.params.as_ref());
            methods::session::spawn(state, opts)
                .await
                .and_then(|r| serde_json::to_value(r).map_err(Error::Json))
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

/// Parse the `session.spawn` params shape — M2 only honours `binary`; the
/// full `Options` deserialiser lands in M3.
fn parse_spawn_params(p: Option<&Value>) -> forge_sdk::Options {
    let raw = p
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::default()));
    let opts = raw
        .get("options")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::default()));
    let binary = opts
        .get("binary")
        .and_then(|v| v.as_str())
        .unwrap_or("claude")
        .to_string();
    forge_sdk::OptionsBuilder::new().binary(binary).build()
}
