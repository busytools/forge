//! Stand-ins for the veto's tests: an endpoint that answers like a host's,
//! and a client that speaks to a real one.
//!
//! **A stub pins the client's own conversation** - the pause it arms, the
//! session it patches, the order of both, and what it does when a step fails.
//! What needs a real Chromium - a document actually running the script - is
//! the live probe, which drives a scratch host through [`Wire`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

/// Every call an endpoint was sent, in the order it arrived.
pub type Calls = Arc<Mutex<Vec<Value>>>;

/// A stand-in browser endpoint, listening until the test process ends.
pub struct Stub {
    pub port: u16,
    pub calls: Calls,
    /// When set, the first script registration is refused: a page whose arm
    /// registered nothing must not be navigated.
    pub refuse_script_once: Arc<std::sync::atomic::AtomicBool>,
    /// When set, the first context patch answers `failed ...`, so the
    /// installer's frozen-prototype arm is exercised.
    pub fail_verdict_once: Arc<std::sync::atomic::AtomicBool>,
}

impl Stub {
    /// The position of the first call to `method`, if one was sent.
    pub fn index(&self, method: &str) -> Option<usize> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .position(|call| call["method"] == method)
    }

    /// How many times `method` was sent.
    pub fn count(&self, method: &str) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|call| call["method"] == method)
            .count()
    }

    /// The first call to `method`, if one was sent.
    pub fn first(&self, method: &str) -> Option<Value> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|call| call["method"] == method)
            .cloned()
    }
}

/// The endpoint a test host answers on.
///
/// **It is not a well-behaved host on purpose**: `Page.enable` and
/// `Runtime.runIfWaitingForDebugger` refuse, so a patch that failed still has
/// to be resumed and both failures have to be logged; every report is sent
/// twice, so a repeated attach or context is visible rather than assumed
/// away.
pub async fn endpoint() -> Result<Stub, String> {
    serve(true).await
}

/// An endpoint whose `/json/version` answers but whose DevTools socket never
/// accepts: a host whose veto cannot be armed.
pub async fn endpoint_without_websocket() -> Result<Stub, String> {
    serve(false).await
}

/// Mark a target armed the way the installer does, for the driver's waits.
pub fn mark_armed(port: u16, target: &str) {
    super::armed_set(port).lock().unwrap_or_else(PoisonError::into_inner).insert(target.to_owned());
}

async fn serve(websocket: bool) -> Result<Stub, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|why| format!("the stub endpoint cannot listen: {why}"))?;
    let port = listener
        .local_addr()
        .map_err(|why| format!("the stub endpoint has no address: {why}"))?
        .port();
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);
    let refuse_script_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fail_verdict_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let refuse = Arc::clone(&refuse_script_once);
    let failing = Arc::clone(&fail_verdict_once);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { continue };
            let recorded = Arc::clone(&recorded);
            let refuse = Arc::clone(&refuse);
            let failing = Arc::clone(&failing);
            tokio::spawn(async move {
                answer(socket, port, websocket, recorded, refuse, failing).await
            });
        }
    });
    Ok(Stub { port, calls, refuse_script_once, fail_verdict_once })
}

/// The reads a [`ListStub`] has queued: each inner list is one answer, and
/// the last repeats.
type ListReads = Arc<Mutex<Vec<Vec<(String, String)>>>>;

/// A host endpoint that answers `/json/list` from a list the test changes.
pub struct ListStub {
    pub port: u16,
    reads: ListReads,
}

impl ListStub {
    /// Replace the page targets the endpoint reports on every read.
    pub fn set(&self, targets: Vec<(String, String)>) {
        *self.reads.lock().unwrap_or_else(PoisonError::into_inner) = vec![targets];
    }

    /// **Answer reads with these lists in order, the last repeating.** A
    /// creation can then be made to appear *after* the driver's snapshot,
    /// which is the only way the wait it takes is observable from outside.
    pub fn sequence(&self, reads: Vec<Vec<(String, String)>>) {
        *self.reads.lock().unwrap_or_else(PoisonError::into_inner) = reads;
    }
}

/// The `/json/list` endpoint a driver test points its waits at.
pub async fn list_endpoint() -> Result<ListStub, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|why| format!("the list endpoint cannot listen: {why}"))?;
    serve_list(listener).await
}

/// The same endpoint on a port of the caller's: a test host's own.
pub async fn list_on(port: u16) -> Result<ListStub, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|why| format!("the list endpoint cannot take port {port}: {why}"))?;
    serve_list(listener).await
}

async fn serve_list(listener: tokio::net::TcpListener) -> Result<ListStub, String> {
    let port = listener
        .local_addr()
        .map_err(|why| format!("the list endpoint has no address: {why}"))?
        .port();
    let reads: ListReads = Arc::new(Mutex::new(Vec::new()));
    let served = Arc::clone(&reads);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { continue };
            let served = Arc::clone(&served);
            tokio::spawn(async move {
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
                let mut body = [0_u8; 2048];
                let _ = socket.read(&mut body).await;
                let answer = if head.starts_with(b"GET /json/version") {
                    json!({ "Browser": "Chrome/154.0" }).to_string()
                } else {
                    let next = {
                        let mut reads = served.lock().unwrap_or_else(PoisonError::into_inner);
                        match reads.len() {
                            0 => Vec::new(),
                            1 => reads[0].clone(),
                            _ => reads.remove(0),
                        }
                    };
                    Value::Array(
                        next.iter()
                            .map(|(id, url)| {
                                json!({ "id": id, "type": "page", "title": "", "url": url })
                            })
                            .collect(),
                    )
                    .to_string()
                };
                let page = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = socket.write_all(page.as_bytes()).await;
            });
        }
    });
    Ok(ListStub { port, reads })
}

async fn answer(
    socket: tokio::net::TcpStream,
    port: u16,
    websocket: bool,
    calls: Calls,
    refuse_script_once: Arc<std::sync::atomic::AtomicBool>,
    fail_verdict_once: Arc<std::sync::atomic::AtomicBool>,
) {
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
    if head.starts_with(b"GET /json/version") {
        let mut socket = socket;
        let mut body = [0_u8; 2048];
        let _ = socket.read(&mut body).await;
        let answer = json!({
            "Browser": "stub",
            "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/browser/stub"),
        })
        .to_string();
        let page = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{answer}",
            answer.len()
        );
        let _ = socket.write_all(page.as_bytes()).await;
        return;
    }
    if !websocket {
        return;
    }
    let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else { return };
    while let Some(Ok(message)) = ws.next().await {
        let Ok(text) = message.into_text() else { continue };
        let Ok(call) = serde_json::from_str::<Value>(&text) else { continue };
        let method = call["method"].as_str().unwrap_or_default().to_owned();
        calls.lock().unwrap_or_else(PoisonError::into_inner).push(json!({
            "method": method,
            "session": call.get("sessionId").cloned(),
            "params": call.get("params").cloned(),
        }));
        let id = call["id"].clone();
        let reply = match method.as_str() {
            "Page.enable" => json!({
                "id": id,
                "error": { "code": -32601, "message": "Page.enable refused" },
            }),
            "Runtime.runIfWaitingForDebugger" => json!({
                "id": id,
                "error": { "code": -32601, "message": "resume refused" },
            }),
            "Runtime.evaluate"
                if fail_verdict_once.swap(false, std::sync::atomic::Ordering::Relaxed) =>
            {
                json!({
                    "id": id,
                    "result": { "result": {
                        "type": "string",
                        "value": "failed TypeError: the prototype is frozen",
                    }},
                })
            }
            "Runtime.evaluate" => json!({
                "id": id,
                "result": { "result": { "type": "string", "value": "patched" } },
            }),
            "Target.attachToTarget" => json!({
                "id": id,
                "result": { "sessionId": "S3" },
            }),
            "Target.getTargets" => json!({
                "id": id,
                "result": { "targetInfos": [
                    { "targetId": "T1", "type": "page", "title": "", "url": "about:blank" },
                ]},
            }),
            "Page.addScriptToEvaluateOnNewDocument"
                if refuse_script_once.swap(false, std::sync::atomic::Ordering::Relaxed) =>
            {
                json!({
                    "id": id,
                    "error": { "code": -32601, "message": "the script is refused" },
                })
            }
            _ => json!({ "id": id, "result": {} }),
        };
        let _ = ws.send(Message::Text(reply.to_string().into())).await;
        if method == "Target.setAutoAttach" {
            let paused = call["params"]["waitForDebuggerOnStart"].as_bool() == Some(true);
            // Everything twice: the client must arm and patch one report each.
            // **Two kinds of target**: a page that was already running (armed
            // in place, nothing to release) and a fresh tab the engine
            // reports as `other` and paused.
            for _ in 0..2 {
                for event in [
                    json!({
                        "method": "Target.attachedToTarget",
                        "params": {
                            "sessionId": "S1",
                            "targetInfo": { "targetId": "T1", "type": "page" },
                            "waitingForDebugger": false,
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
                    json!({
                        "method": "Target.attachedToTarget",
                        "params": {
                            "sessionId": "S2",
                            "targetInfo": { "targetId": "T2", "type": "other" },
                            "waitingForDebugger": paused,
                        },
                    }),
                ] {
                    let _ = ws.send(Message::Text(event.to_string().into())).await;
                }
            }
            // **A context that is not the page's own is skipped.**
            let other_world = json!({
                "method": "Runtime.executionContextCreated",
                "sessionId": "S1",
                "params": { "context": {
                    "id": 55,
                    "origin": "http://localhost",
                    "auxData": { "isDefault": false, "type": "isolated" },
                }},
            });
            let _ = ws.send(Message::Text(other_world.to_string().into())).await;
            // **A worker is not a page**: it must be skipped, not armed.
            let worker = json!({
                "method": "Target.attachedToTarget",
                "params": {
                    "sessionId": "SW1",
                    "targetInfo": { "targetId": "W1", "type": "service_worker" },
                    "waitingForDebugger": false,
                },
            });
            let _ = ws.send(Message::Text(worker.to_string().into())).await;
            // **A sentinel the burst's last word**: its context is the final
            // frame, so a test that waits for it has seen the whole burst
            // handled, duplicates included.
            let sentinel = json!({
                "method": "Runtime.executionContextCreated",
                "sessionId": "S1",
                "params": { "context": {
                    "id": 99,
                    "origin": "http://localhost",
                    "auxData": { "isDefault": true, "type": "default" },
                }},
            });
            let _ = ws.send(Message::Text(sentinel.to_string().into())).await;
        }
    }
}

/// One DevTools connection to a live host, carrying the probe's own calls.
pub struct Wire {
    sink: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    replies: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next: u64,
}

impl Wire {
    /// Connect at the host's own browser endpoint.
    pub async fn connect(port: u16) -> Result<Self, String> {
        let url = super::spawn::browser_websocket(port)
            .await
            .ok_or_else(|| format!("the host on {port} names no browser endpoint"))?;
        let (socket, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|why| format!("the host's endpoint did not accept: {why}"))?;
        let (sink, mut source) = socket.split();
        let replies: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader = Arc::clone(&replies);
        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = source.next().await {
                let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                let Some(id) = frame["id"].as_u64() else { continue };
                let waiter = reader.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(frame);
                }
            }
            reader.lock().unwrap_or_else(PoisonError::into_inner).clear();
        });
        Ok(Self { sink, replies, next: 0 })
    }

    /// One call, answered.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let (waiter, answered) = oneshot::channel();
        self.replies.lock().unwrap_or_else(PoisonError::into_inner).insert(id, waiter);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.to_owned());
        }
        self.sink
            .send(Message::Text(message.to_string().into()))
            .await
            .map_err(|why| format!("{method} could not be sent: {why}"))?;
        // **Bounded**, so a probe against an unpatched page (whose call
        // raises the engine's dialog and never settles) fails rather than
        // hanging the test that drives it.
        let frame = tokio::time::timeout(Duration::from_secs(10), answered)
            .await
            .map_err(|_| format!("{method} did not answer within 10 s"))?
            .map_err(|_| format!("{method}: the endpoint closed before it answered"))?;
        match frame.get("error") {
            Some(error) => Err(format!("{method}: {error}")),
            None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
        }
    }

    /// Every page target the host reports, as (id, url).
    pub async fn targets(&mut self) -> Result<Vec<(String, String)>, String> {
        let answer = self.call("Target.getTargets", json!({}), None).await?;
        let mut pages = Vec::new();
        for info in answer["targetInfos"].as_array().into_iter().flatten() {
            if info["type"].as_str() != Some("page") {
                continue;
            }
            if let (Some(id), Some(url)) = (info["targetId"].as_str(), info["url"].as_str()) {
                pages.push((id.to_owned(), url.to_owned()));
            }
        }
        Ok(pages)
    }

    /// Open a tab and answer its target id.
    pub async fn create_target(&mut self, url: &str) -> Result<String, String> {
        let answer = self.call("Target.createTarget", json!({ "url": url }), None).await?;
        answer["targetId"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "Target.createTarget answered no target".to_owned())
    }

    /// Attach to one tab and navigate it.
    pub async fn navigate(&mut self, target: &str, url: &str) -> Result<(), String> {
        let session = self.attach(target).await?;
        self.call("Page.navigate", json!({ "url": url }), Some(&session)).await?;
        Ok(())
    }

    /// Attach to one tab and answer its session.
    async fn attach(&mut self, target: &str) -> Result<String, String> {
        let attached = self
            .call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)
            .await?;
        attached["sessionId"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "Target.attachToTarget answered no session".to_owned())
    }

    /// Attach to one tab, evaluate `expression`, and answer its value.
    pub async fn evaluate(&mut self, target: &str, expression: &str) -> Result<Value, String> {
        let session = self.attach(target).await?;
        let answer = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true,
                }),
                Some(&session),
            )
            .await?;
        Ok(answer["result"]["value"].clone())
    }
}
