//! The takeover's live view: the browser the agents drive, shown inside the
//! client.
//!
//! **One browsing state.** The person's view and the sessions' tools are the
//! same Chromium: this module opens its OWN CDP session to the browser's page
//! target, starts `Page.startScreencast` there, and forwards input back down
//! the same session. Nothing here owns or relaunches a browser - the vendored
//! Chrome and its drivers are exactly what they were.
//!
//! **Frames are acknowledged.** CDP stops the stream when a frame goes
//! unacknowledged, so every frame is acked before it is handed on.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// How many pre-attach input messages are held. The real window is a
/// handshake's length; the cap only bounds a stream nothing will ever flush.
const HELD_INPUT_CAP: usize = 256;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// One frame as the view draws it: a whole JPEG in base64 (the view's mime is
/// `image/jpeg`), with the page's own pixel size so input can be mapped back
/// into it. JPEG rather than PNG because a retina-size PNG is megabytes per
/// frame, and the encode cost lands on the same browser the person is using.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Frame {
    pub data: String,
    pub width: u32,
    pub height: u32,
}

/// A live view: input goes in, frames come out, and stopping aborts the
/// session it opened.
pub struct Live {
    to_session: mpsc::UnboundedSender<Value>,
    session: tokio::task::JoinHandle<()>,
}

impl Live {
    /// One input message, as CDP wants it: `Input.dispatchMouseEvent`,
    /// `Input.dispatchKeyEvent` or `Input.insertText` with its params.
    pub fn input(&self, method: &str, params: Value) {
        let _ = self.to_session.send(json!({ "method": method, "params": params }));
    }

    /// Take the view down: this CDP session closes, the browser keeps running.
    pub fn stop(self) {
        self.session.abort();
    }
}

/// Open the view on the browser whose debug endpoint is `endpoint` (the
/// websocket URL the port file names), delivering frames to `on_frame`.
///
/// **`with_frames` is where the platforms part.** The desktop client
/// renders the browser natively, so its session carries INPUT and nothing
/// else - a screencast whose frames nobody draws is pure cost. The
/// platforms that still draw frames (the phone) ask for them.
///
/// Fails rather than pretending: a debug port that will not open is the
/// dock's cue to say the view is not up.
pub async fn start(
    endpoint: &str,
    with_frames: bool,
    on_frame: impl Fn(Frame) + Send + 'static,
) -> Result<Live, String> {
    let (socket, _) = tokio_tungstenite::connect_async(endpoint)
        .await
        .map_err(|why| format!("the browser's debug port would not open: {why}"))?;
    let (mut sink, mut stream) = socket.split();
    let next_id = Arc::new(AtomicU64::new(1));
    let (to_session, mut from_callers) = mpsc::unbounded_channel::<Value>();
    let ids = Arc::clone(&next_id);

    let session = tokio::spawn(async move {
        let id = || ids.fetch_add(1, Ordering::Relaxed);
        let mut page: Option<String> = None;
        // **Input that arrives before the page attach is held, not dropped.**
        // The view's own first act is the viewport override, and the attach
        // handshake is still in flight when it lands - dropping it would
        // leave the page at the browser's default size, which the person
        // sees as a pixelated image stretched across the stage.
        let mut held: Vec<Value> = Vec::new();

        let attach = json!({
            "id": id(),
            "method": "Target.setAutoAttach",
            "params": { "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true },
        });
        if sink.send(Message::Text(attach.to_string().into())).await.is_err() {
            return;
        }

        loop {
            let text = tokio::select! {
                incoming = stream.next() => match incoming {
                    Some(Ok(Message::Text(text))) => text.to_string(),
                    Some(Ok(_)) => continue,
                    _ => break,
                },
                out = from_callers.recv() => match out {
                    Some(message) => {
                        let Some(attached) = page.as_deref() else {
                            // Drop-newest at the cap: the earliest messages
                            // (the override) are the ones that must survive.
                            if held.len() < HELD_INPUT_CAP {
                                held.push(message);
                            }
                            continue;
                        };
                        let method = message.get("method").cloned().unwrap_or(Value::Null);
                        let params = message.get("params").cloned().unwrap_or(json!({}));
                        let message = json!({
                            "id": id(), "method": method, "params": params,
                            "sessionId": attached,
                        });
                        if sink.send(Message::Text(message.to_string().into())).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    None => break,
                },
            };
            let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
            match message.get("method").and_then(Value::as_str) {
                Some("Target.attachedToTarget") => {
                    let Some(params) = message.get("params") else { continue };
                    let kind = params.pointer("/targetInfo/type").and_then(Value::as_str);
                    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
                        continue;
                    };
                    if kind != Some("page") || page.is_some() {
                        continue;
                    }
                    page = Some(session_id.to_owned());
                    let mut opening = vec![
                        ("Page.enable", json!({})),
                        // The target is held for the debugger at attach; this
                        // is what lets it run.
                        ("Runtime.runIfWaitingForDebugger", json!({})),
                    ];
                    if with_frames {
                        opening.push((
                            "Page.startScreencast",
                            json!({ "format": "jpeg", "quality": 70, "everyNthFrame": 1 }),
                        ));
                    }
                    for (method, params) in opening {
                        let message = json!({
                            "id": id(), "method": method, "params": params,
                            "sessionId": session_id,
                        });
                        if sink.send(Message::Text(message.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                    for message in held.drain(..) {
                        let method = message.get("method").cloned().unwrap_or(Value::Null);
                        let params = message.get("params").cloned().unwrap_or(json!({}));
                        let message = json!({
                            "id": id(), "method": method, "params": params,
                            "sessionId": session_id,
                        });
                        if sink.send(Message::Text(message.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                }
                Some("Page.screencastFrame") if with_frames => {
                    let Some(params) = message.get("params") else { continue };
                    let ack = json!({
                        "id": id(),
                        "method": "Page.screencastFrameAck",
                        "params": { "sessionId": params.get("sessionId").cloned().unwrap_or(Value::Null) },
                        "sessionId": page.clone().unwrap_or_default(),
                    });
                    if sink.send(Message::Text(ack.to_string().into())).await.is_err() {
                        break;
                    }
                    let Some(data) = params.get("data").and_then(Value::as_str) else { continue };
                    let metadata = params.get("metadata");
                    let width = metadata
                        .and_then(|m| m.get("deviceWidth"))
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0) as u32;
                    let height = metadata
                        .and_then(|m| m.get("deviceHeight"))
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0) as u32;
                    on_frame(Frame { data: data.to_owned(), width, height });
                }
                _ => {}
            }
        }
    });

    Ok(Live { to_session, session })
}
