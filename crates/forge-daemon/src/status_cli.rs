//! `forge-daemon status` subcommand. Connects to a local forge-daemon over loopback,
//! issues `daemon.status`, prints the JSON result to stdout.

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use crate::Error;
use crate::jsonrpc::{Request, Response};

/// Query a forge-daemon at `addr` (e.g. `127.0.0.1:7373`) and return the
/// pretty-printed JSON status.
///
/// # Errors
///
/// Connection / serialisation / protocol errors.
pub async fn query(addr: &str) -> Result<String, Error> {
    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url)
        .await
        .map_err(|e| Error::InternalError(format!("connect {addr}: {e}")))?;

    let req = Request::new("daemon.status", serde_json::json!({}), serde_json::json!(1));
    ws.send(WsMsg::Text(serde_json::to_string(&req)?))
        .await
        .map_err(|e| Error::InternalError(format!("send: {e}")))?;

    // Drain notifications until we see a response with id 1.
    loop {
        let msg = ws
            .next()
            .await
            .ok_or_else(|| Error::InternalError("ws closed before response".into()))?;
        let msg = msg.map_err(|e| Error::InternalError(format!("recv: {e}")))?;
        let WsMsg::Text(text) = msg else { continue };
        let v: Value = serde_json::from_str(&text)?;
        if v.get("id").is_some() {
            let resp: Response = serde_json::from_value(v)?;
            if let Some(err) = resp.error {
                return Err(Error::InternalError(format!(
                    "daemon.status: {} (code {})",
                    err.message, err.code
                )));
            }
            let result = resp.result.ok_or_else(|| {
                Error::InternalError(
                    "daemon returned response with neither result nor error".into(),
                )
            })?;
            return Ok(serde_json::to_string_pretty(&result)?);
        }
    }
}

/// Run the `forge-daemon status` subcommand — print the result of [`query`] to stdout.
///
/// # Errors
///
/// Any error from [`query`].
pub async fn run(addr: &str) -> Result<(), Error> {
    let out = query(addr).await?;
    println!("{out}");
    Ok(())
}
