#![cfg(target_os = "macos")]
//! The passkey veto's CDP conversation, against a stand-in endpoint.
//!
//! What a stub can pin is the client's own decisions: the browser endpoint it
//! asks for, the targets it arms, and the script it registers. What needs a
//! real Chromium - a page actually running that script - belongs to the probe
//! and the look.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::Message;

/// Serve `/json/version` and one DevTools websocket, recording every call.
async fn endpoint(listener: tokio::net::TcpListener, port: u16, seen: Arc<Mutex<Vec<Value>>>) {
    loop {
        let Ok((socket, _)) = listener.accept().await else { continue };
        let seen = Arc::clone(&seen);
        tokio::spawn(async move {
            // Peek the request line so the websocket handshake reaches
            // `accept_async` with its bytes untouched.
            let mut head = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let Ok(n) = socket.peek(&mut chunk).await else { return };
                if n == 0 {
                    return;
                }
                head.extend_from_slice(&chunk[..n]);
                if head.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let request = String::from_utf8_lossy(&head).into_owned();
            if request.starts_with("GET /json/version") {
                let mut socket = socket;
                let mut sink = [0_u8; 2048];
                let _ = socket.read(&mut sink).await;
                let body = json!({
                    "Browser": "stub",
                    "webSocketDebuggerUrl":
                        format!("ws://127.0.0.1:{port}/devtools/browser/stub"),
                })
                .to_string();
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                     {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(answer.as_bytes()).await;
                return;
            }

            let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else { return };
            while let Some(Ok(message)) = ws.next().await {
                let Ok(text) = message.into_text() else { continue };
                let Ok(call) = serde_json::from_str::<Value>(&text) else { continue };
                let method = call["method"].as_str().unwrap_or_default().to_owned();
                let id = call["id"].clone();
                seen.lock().expect("the record").push(json!({
                    "method": method,
                    "session": call.get("sessionId").cloned(),
                    "params": call.get("params").cloned(),
                }));
                // The page session of a target discovered later comes back in
                // this call's own answer; the veto must arm it.
                let target_id = call["params"]["targetId"].as_str().unwrap_or_default();
                let answer = if method == "Target.attachToTarget" {
                    let session = if target_id == "T3" { "S3P" } else { "S2" };
                    json!({ "id": id, "result": { "sessionId": session } }).to_string()
                } else {
                    json!({ "id": id, "result": {} }).to_string()
                };
                let _ = ws.send(Message::Text(answer.into())).await;
                if method == "Target.attachToTarget" && target_id == "T2" {
                    let event = json!({
                        "method": "Runtime.executionContextCreated",
                        "sessionId": "S2",
                        "params": { "context": {
                            "id": 8,
                            "origin": "http://localhost",
                            "auxData": { "isDefault": true, "type": "default" },
                        }},
                    });
                    let _ = ws.send(Message::Text(event.to_string().into())).await;
                }
                if method == "Target.setDiscoverTargets" {
                    // A page target attaches, then reports its main world.
                    for event in [
                        json!({
                            "method": "Target.attachedToTarget",
                            "params": {
                                "sessionId": "S1",
                                "targetInfo": { "targetId": "T1", "type": "page" },
                                "waitingForDebugger": true,
                            },
                        }),
                        json!({
                            "method": "Runtime.executionContextCreated",
                            "sessionId": "S1",
                            "params": { "context": {
                                "id": 7,
                                "origin": "http://localhost",
                                "auxData": { "isDefault": true, "type": "default" },
                            }},
                        }),
                        // A target that appears later; its page session comes
                        // back from the attach the veto sends in answer.
                        json!({
                            "method": "Target.targetCreated",
                            "params": { "targetInfo": {
                                "targetId": "T2",
                                "type": "page",
                                "url": "about:blank",
                            }},
                        }),
                        // A target CEF created late attaches as `other`, paused;
                        // the page session comes from re-attaching it.
                        json!({
                            "method": "Target.attachedToTarget",
                            "params": {
                                "sessionId": "S3",
                                "targetInfo": { "targetId": "T3", "type": "other" },
                                "waitingForDebugger": true,
                            },
                        }),
                    ] {
                        let _ = ws.send(Message::Text(event.to_string().into())).await;
                    }
                }
            }
        });
    }
}

/// The live probe: point `FORGE_VETO_PORT` at a running host and this calls
/// the real installer, holding it while a person drives the pages. Ignored in
/// the gate; its run and its reading are recorded on #2483.
#[tokio::test]
#[ignore = "needs a running host; run with FORGE_VETO_PORT"]
async fn the_live_veto_installs_on_a_host() {
    let port: u16 = std::env::var("FORGE_VETO_PORT")
        .expect("FORGE_VETO_PORT names the host to veto")
        .parse()
        .expect("FORGE_VETO_PORT is a port");
    let installer = tokio::spawn(forge_client::browser::cef::veto::install(port));
    tokio::time::sleep(Duration::from_secs(45)).await;
    installer.abort();
}

/// **Every page target is armed, not just the first.** The veto asks the
/// browser endpoint for sessions and registers its script on each page it
/// attaches; the recorded calls are the client's own decisions, and the
/// script's own shape is pinned with them.
#[tokio::test]
async fn the_veto_arms_every_page_target() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("the address").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(endpoint(listener, port, Arc::clone(&seen)));

    let install = tokio::spawn(forge_client::browser::cef::veto::install(port));
    let deadline = Instant::now() + Duration::from_secs(5);
    let calls = loop {
        let calls = seen.lock().expect("the record").clone();
        let evaluated = calls
            .iter()
            .any(|call| call["method"] == "Runtime.evaluate" && call["params"]["contextId"] == 8);
        if evaluated || Instant::now() >= deadline {
            break calls;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    install.abort();

    let find = |method: &str| calls.iter().find(|call| call["method"] == method);
    let arming = find("Page.addScriptToEvaluateOnNewDocument").expect("the script is registered");
    assert_eq!(arming["session"], "S1", "the page target's session is the one armed");
    let source = arming["params"]["source"].as_str().expect("a script");
    assert!(source.contains("CredentialsContainer.prototype"), "the prototype is patched");
    assert!(source.contains("NotAllowedError"), "the refusal a page handles");
    assert!(source.contains("configurable: false"), "the patch is not replaceable");
    assert!(find("Page.enable").is_some(), "the target's page domain is enabled");
    assert!(find("Runtime.enable").is_some(), "the target's contexts are reported");
    assert!(find("Target.setDiscoverTargets").is_some(), "existing targets are discovered");
    assert!(find("Target.setAutoAttach").is_some(), "new targets wait for the veto");
    let evaluate = find("Runtime.evaluate").expect("an existing context is patched");
    assert_eq!(evaluate["session"], "S1", "the patch rides the page's session");
    assert_eq!(evaluate["params"]["contextId"], 7, "the reported context is the one patched");
    assert_eq!(
        evaluate["params"]["expression"].as_str(),
        Some(source),
        "the same veto script runs in the existing context"
    );

    // **A target created later is armed from its attach answer.** CEF gives a
    // new page's session in `Target.attachToTarget`'s reply, not an event;
    // the page already ran, so the script is registered and the document
    // restarted under it.
    let attach = find("Target.attachToTarget").expect("a later target is attached");
    assert_eq!(attach["params"]["targetId"], "T2", "the created target is the one attached");
    let second = calls
        .iter()
        .find(|call| {
            call["method"] == "Page.addScriptToEvaluateOnNewDocument" && call["session"] == "S2"
        })
        .expect("the created target's session is armed");
    assert_eq!(second["params"]["source"].as_str(), Some(source), "the same veto, on the tab");
    let reload = calls
        .iter()
        .find(|call| call["method"] == "Page.reload" && call["session"] == "S2")
        .expect("the created target's document restarts under the veto");
    assert_eq!(reload["session"], "S2", "only the new target is restarted");
    let late = calls
        .iter()
        .find(|call| call["method"] == "Runtime.evaluate" && call["session"] == "S2")
        .expect("the created target's context is patched");
    assert_eq!(late["params"]["contextId"], 8, "its reported context is the one patched");

    // **The `other` target a late creation reports is armed before it runs.**
    // Its page session comes from the re-attach; the paused session is only
    // released after that session carries the script.
    assert!(
        calls.iter().any(|call| call["method"] == "Page.addScriptToEvaluateOnNewDocument"
            && call["session"] == "S3P"),
        "the late target's page session is armed"
    );
    assert!(
        calls
            .iter()
            .any(|call| call["method"] == "Runtime.runIfWaitingForDebugger"
                && call["session"] == "S3"),
        "the paused target is released after arming"
    );
}
