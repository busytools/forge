//! The ask path: a session's browser tool call, answered by this process.
//!
//! **The page is not on this path.** Before, a `browser_ask` frame had to
//! reach the webview, which invoked `browser_call`, which ran the host - so a
//! parked page starved every ask (the 2026-10-10 catch). Now the socket
//! hands the ask straight to the host that already owns the drivers, and the
//! webview is not consulted at all. What the webview still owns is the
//! hand-off surfaces, which are their own asks answered by commands, not by
//! `browser_ask` frames.
//!
//! An ask's answer is the parts the tool returned, with an image's bytes in
//! their own frames right after the answer that declared them - the socket's
//! own contract, kept here so the mapping between the host's reply and the
//! wire lives in one place.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::browser::BrowserHost;
use crate::browser::driver::ReplyPart;
use crate::browser::profiles::Seat;
use crate::socket::Socket;

/// Answer one `browser_ask` frame on a task of its own, so a slow call never
/// holds the socket's reading.
pub fn answer(frame: &Value, socket: &Socket, host: &Arc<BrowserHost>) {
    let (frame, socket, host) = (frame.clone(), socket.clone(), Arc::clone(host));
    tokio::spawn(async move {
        run(&frame, &socket, &host).await;
    });
}

/// The ask path itself: parse the frame, run the tool, answer. On whatever
/// task the caller has - the app half awaits this inside its own counting, so
/// it calls here rather than through `answer`.
pub async fn run(frame: &Value, socket: &Socket, host: &Arc<BrowserHost>) {
    let Some(id) = frame.get("id").and_then(Value::as_u64) else { return };
    let seat: Result<Seat, _> =
        serde_json::from_value(frame.get("seat").cloned().unwrap_or(Value::Null));
    let Ok(seat) = seat else {
        socket.answer(id, json!([]), Some("the ask carried no readable seat".to_owned()));
        return;
    };
    let tool = frame.get("tool").and_then(Value::as_str).unwrap_or_default().to_owned();
    let args = frame.get("args").cloned().unwrap_or(Value::Null);
    match host.call(&seat, &tool, args).await {
        Ok(parts) => {
            let (declared, images) = declare(&parts);
            socket.answer(id, Value::Array(declared), None);
            // The frames follow the answer that declared them, in the order
            // the parts are listed - what the server reads.
            for bytes in images {
                socket.send_binary(bytes);
            }
        }
        Err(why) => socket.answer(id, json!([]), Some(why)),
    }
}

/// The parts as the answer frame carries them, and the images' bytes to
/// follow as binary frames. A text part is its own words; an image part is
/// its mime type here and its bytes in a frame.
fn declare(parts: &[ReplyPart]) -> (Vec<Value>, Vec<Vec<u8>>) {
    let mut declared = Vec::with_capacity(parts.len());
    let mut images = Vec::new();
    for part in parts {
        match part {
            ReplyPart::Text { text } => {
                declared.push(json!({ "type": "text", "text": text }));
            }
            ReplyPart::Image { mime_type, data_base64 } => {
                declared.push(json!({ "type": "image", "mime_type": mime_type }));
                // The driver's bytes cross as base64; the wire wants them raw.
                match base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    data_base64,
                ) {
                    Ok(bytes) => images.push(bytes),
                    Err(_) => images.push(Vec::new()),
                }
            }
        }
    }
    (declared, images)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The declared parts and the frames to follow, in order: a text part
    /// carries its words, an image part its mime, and the bytes ride behind.
    #[test]
    fn a_reply_declares_its_parts_and_sends_image_bytes_behind() {
        let parts = vec![
            ReplyPart::Text { text: "hello".to_owned() },
            ReplyPart::Image { mime_type: "image/png".to_owned(), data_base64: "aGk=".to_owned() },
            ReplyPart::Text { text: "done".to_owned() },
        ];
        let (declared, images) = declare(&parts);
        assert_eq!(declared[0], json!({ "type": "text", "text": "hello" }));
        assert_eq!(declared[1], json!({ "type": "image", "mime_type": "image/png" }));
        assert_eq!(declared[2], json!({ "type": "text", "text": "done" }));
        assert_eq!(images, vec![b"hi".to_vec()], "the image's bytes are behind its declaration");
    }

    /// **An ask reaches the host and its failure is the answer.** With a host
    /// that cannot act, the ask is still answered - with the host's reason -
    /// rather than left hanging, and the reason is the host's sentence.
    #[tokio::test]
    async fn an_ask_is_answered_with_the_hosts_own_reason() {
        let mut stub = crate::socket::tests::stub().await;
        let socket = crate::socket::tests::open_connected(&stub).await;
        let host = Arc::new(BrowserHost::unavailable("the stack is not there".to_owned()));
        let ask = json!({
            "kind": "browser_ask",
            "id": 7,
            "seat": { "org": "Busytools", "project": "forge", "label": "lead" },
            "tool": "browser_navigate",
            "args": { "url": "https://example.com" },
        });
        answer(&ask, &socket, &host);
        let said = stub.heard_until(|v| v["kind"] == "browser_answer").await;
        assert_eq!(said["id"], 7);
        assert_eq!(said["error"], "the stack is not there");
        assert_eq!(said["parts"], json!([]));
    }
}
