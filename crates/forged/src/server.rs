//! WebSocket server — bind, accept, dispatch.

use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tracing::{debug, info, warn};

use crate::Error;
use crate::connection::{Connection, ConnectionId};
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

    let (tx, _rx) = mpsc::unbounded_channel();
    let conn = Connection {
        id: ConnectionId::new(),
        name: None,
        outbound: tx,
    };
    *state.connected_clients.lock() += 1;

    let r = process_messages(ws, &conn, &state).await;

    *state.connected_clients.lock() -= 1;
    r
}

async fn process_messages(
    mut ws: WebSocketStream<TcpStream>,
    conn: &Connection,
    state: &DaemonState,
) -> Result<(), Error> {
    // Send the initial client.identify notification.
    let identify = Notification::new(
        "client.identify",
        serde_json::json!({
            "connection_id": conn.id.0,
            "server_version": env!("CARGO_PKG_VERSION"),
            "server_build": option_env!("FORGED_BUILD_SHA").unwrap_or("dev"),
        }),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&identify)?))
        .await
        .map_err(|e| Error::InternalError(format!("ws send: {e}")))?;

    while let Some(msg) = ws.next().await {
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
                let resp =
                    Response::error(Value::Null, Error::ParseError(e.to_string()).to_jsonrpc());
                let _ = ws.send(WsMsg::Text(serde_json::to_string(&resp)?)).await;
                continue;
            }
        };

        let resp = dispatch(&req, state).await;
        ws.send(WsMsg::Text(serde_json::to_string(&resp)?))
            .await
            .map_err(|e| Error::InternalError(format!("ws send: {e}")))?;
    }

    Ok(())
}

async fn dispatch(req: &Request, state: &DaemonState) -> Response {
    match req.method.as_str() {
        "daemon.status" => match methods::daemon::status(state).await {
            Ok(s) => match serde_json::to_value(s) {
                Ok(v) => Response::success(req.id.clone(), v),
                Err(e) => Response::error(req.id.clone(), Error::Json(e).to_jsonrpc()),
            },
            Err(e) => Response::error(req.id.clone(), e.to_jsonrpc()),
        },
        other => Response::error(
            req.id.clone(),
            Error::MethodNotFound(other.to_string()).to_jsonrpc(),
        ),
    }
}
