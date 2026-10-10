//! The client's socket: the desktop's own connection to a running forge.
//!
//! **The connection lives in this process, not in the webview.** A parked
//! page - the display off, the screen locked, a window never shown - stops
//! consuming its WebSocket, and every ask it has not read dies with it (the
//! 2026-10-10 catch: 12 asks timed out at the 200s bound while both the
//! host and the drivers were healthy). So the socket moves here, where
//! nothing parks, and the webview becomes a reader of what this half holds.
//!
//! It is a port of the web client's `socket.ts` for the desktop, and the
//! semantics are the TS tests': a reconnect declares every held
//! subscription again with the role flags it carried, a refusal is
//! attributed to the oldest subscribe still waiting, and a drop fails
//! whatever a command was holding. The web client at forge.hub has no
//! Tauri process behind it and keeps `socket.ts`; only the desktop selects
//! this half.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// The first reconnect wait; it doubles up to the ceiling and resets on an
/// open, exactly as `socket.ts`'s `retryDelay` does.
const RETRY_MS: u64 = 50;
const MAX_RETRY_MS: u64 = 2000;

/// Where the connection is, as the page's status line reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Open,
    Closed,
}

/// One thing the socket has to say to its owner.
#[derive(Clone, Debug)]
pub enum SocketEvent {
    /// The connection opened, dropped, or was closed by our own hand.
    Status(Status),
    /// A server frame, verbatim: the greeting, snapshots, updates, pages,
    /// asks, devices and errors alike. The receiver reads `kind`.
    Message(Value),
    /// The browser role moved - granted, taken, or released with the drop.
    Role { hosting: bool },
    /// A subscribe the core refused, attributed to the oldest ask still
    /// waiting, exactly as `socket.ts`'s queue does.
    Refused { key: String, why: String },
    /// A frame that could not be read as JSON. Reading goes on: a frame we
    /// cannot parse is not a reason to lose the ones after it.
    Malformed(String),
}

#[cfg(test)]
impl SocketEvent {
    fn kind(&self) -> &'static str {
        match self {
            Self::Status(_) => "status",
            Self::Message(_) => "message",
            Self::Role { .. } => "role",
            Self::Refused { .. } => "refused",
            Self::Malformed(_) => "malformed",
        }
    }
}

/// One subscription this side believes the server holds, in first-asked
/// order, with the declaration it was asked with. A later subscribe for the
/// same subject raises the declaration but keeps the place in line.
struct Held {
    key: String,
    what: Value,
    answering: bool,
    browser: bool,
}

impl Held {
    fn subscribe_frame(&self) -> Message {
        Message::Text(
            json!({
                "kind": "subscribe",
                "what": self.what,
                "answering": self.answering,
                "browser": self.browser,
            })
            .to_string()
            .into(),
        )
    }
}

/// The held list as one place: a fresh subject appends, a repeat raises its
/// flags in place, an unsubscribe drops its entry. Both the connected loop
/// and the disconnected wait fold through here, so their bookkeeping cannot
/// drift.
fn hold(held: &mut Vec<Held>, what: Value, answering: bool, browser: bool) -> Option<String> {
    let key = subject_key(&what);
    if key.is_empty() {
        return None;
    }
    match held.iter_mut().find(|h| h.key == key) {
        Some(h) => {
            h.answering |= answering;
            h.browser |= browser;
        }
        None => held.push(Held { key: key.clone(), what, answering, browser }),
    }
    Some(key)
}

fn unhold(held: &mut Vec<Held>, awaiting: &mut Vec<String>, what: &Value) {
    let key = subject_key(what);
    held.retain(|h| h.key != key);
    awaiting.retain(|k| k != &key);
}

enum Outbound {
    Subscribe {
        what: Value,
        answering: bool,
        browser: bool,
    },
    Unsubscribe {
        what: Value,
    },
    More {
        conversation: Value,
        before: Option<String>,
        turns: u64,
    },
    Devices,
    Dispatch {
        command: Value,
        reply: Option<u64>,
        sent: oneshot::Sender<Result<(), String>>,
        replied: Option<oneshot::Sender<Result<Value, String>>>,
    },
    Answer {
        id: u64,
        parts: Value,
        error: Option<String>,
    },
    TakeRole,
    Binary(Vec<u8>),
    Close,
}

/// The transport's handle: what the app half calls, and where its events land.
#[derive(Clone)]
pub struct Socket {
    tx: mpsc::UnboundedSender<Outbound>,
    next_reply: Arc<AtomicU64>,
}

impl Socket {
    /// Dial `url` and keep the connection up for as long as this handle
    /// lives. Events land on `events`; the task ends on [`Socket::close`].
    pub fn open(url: String, events: mpsc::UnboundedSender<SocketEvent>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let next_reply = Arc::new(AtomicU64::new(1));
        let socket = Self { tx: tx.clone(), next_reply: Arc::clone(&next_reply) };
        tokio::spawn(run(url, events, rx));
        socket
    }

    /// Watch a subject, declaring whether this client can answer its prompts
    /// and whether it hosts the browser. Both flags only ever rise while the
    /// connection lives, and the flags stated here are what a reconnect
    /// re-asks with.
    pub fn subscribe(&self, what: Value, answering: bool, browser: bool) {
        let _ = self.tx.send(Outbound::Subscribe { what, answering, browser });
    }

    /// Give one subscription back.
    pub fn unsubscribe(&self, what: Value) {
        let _ = self.tx.send(Outbound::Unsubscribe { what });
    }

    /// Ask for more turns of a conversation.
    pub fn more(&self, conversation: Value, before: Option<String>, turns: u64) {
        let _ = self.tx.send(Outbound::More { conversation, before, turns });
    }

    /// The dictation devices, answered by a `devices` frame.
    pub fn devices(&self) {
        let _ = self.tx.send(Outbound::Devices);
    }

    /// Send one command. With `reply` the call awaits the core's reply
    /// (`reply_to` is minted here, so correlation needs no caller
    /// bookkeeping); without it the call answers once the frame is on the
    /// wire. Err means the command was NOT sent - a command is refused
    /// rather than sent nowhere.
    pub async fn dispatch(&self, command: Value, reply: bool) -> Result<Option<Value>, String> {
        let (sent, sent_rx) = oneshot::channel();
        if reply {
            let id = self.next_reply.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.tx
                .send(Outbound::Dispatch { command, reply: Some(id), sent, replied: Some(tx) })
                .map_err(|_| "the socket is closed".to_owned())?;
            sent_rx.await.map_err(|_| "the socket is closed".to_owned())??;
            return rx
                .await
                .map_err(|_| "the socket dropped before answering".to_owned())?
                .map(Some);
        }
        self.tx
            .send(Outbound::Dispatch { command, reply: None, sent, replied: None })
            .map_err(|_| "the socket is closed".to_owned())?;
        sent_rx.await.map_err(|_| "the socket is closed".to_owned())??;
        Ok(None)
    }

    /// Answer one browser ask: the parts it returned, or the reason it failed.
    pub fn answer(&self, id: u64, parts: Value, error: Option<String>) {
        let _ = self.tx.send(Outbound::Answer { id, parts, error });
    }

    /// Claim the browser role from whoever holds it.
    pub fn take_role(&self) {
        let _ = self.tx.send(Outbound::TakeRole);
    }

    /// One binary frame: a dictation take's audio, or an image an answer
    /// carried. It rides under the answer that declared it, in order.
    pub fn send_binary(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(Outbound::Binary(bytes));
    }

    /// End the connection for good: no retry follows.
    pub fn close(&self) {
        let _ = self.tx.send(Outbound::Close);
    }
}

/// A subject as the TS side keys it: `home` / `usage` / `dictate_models`, or
/// `session:org\u{0}project\u{0}label`.
fn subject_key(what: &Value) -> String {
    if let Some(s) = what.as_str() {
        return s.to_owned();
    }
    let Some(session) = what.get("session") else { return String::new() };
    let (Some(org), Some(project), Some(label)) = (
        session.get("org").and_then(Value::as_str),
        session.get("project").and_then(Value::as_str),
        session.get("label").and_then(Value::as_str),
    ) else {
        return String::new();
    };
    format!("session:{org}\u{0}{project}\u{0}{label}")
}

/// Apply one outbound message taken while there is NO connection. Only the
/// local bookkeeping can move: a subscribe or unsubscribe updates the held
/// list (the reconnect re-asks it), a dispatch is refused, and an answer,
/// take or frame has nowhere to go and is dropped - which is what
/// `socket.ts`'s own `isOpen()` guards do. Answers `true` when the task
/// should end.
fn apply_offline(next: Outbound, held: &mut Vec<Held>, awaiting: &mut Vec<String>) -> bool {
    match next {
        Outbound::Close => true,
        Outbound::Subscribe { what, answering, browser } => {
            if let Some(key) = hold(held, what, answering, browser) {
                awaiting.push(key);
            }
            false
        }
        Outbound::Unsubscribe { what } => {
            unhold(held, awaiting, &what);
            false
        }
        Outbound::Dispatch { sent, .. } => {
            let _ = sent.send(Err("the socket is not open".to_owned()));
            false
        }
        Outbound::More { .. }
        | Outbound::Devices
        | Outbound::Answer { .. }
        | Outbound::TakeRole
        | Outbound::Binary(_) => false,
    }
}

/// The owning task: connect, read, retry - for the life of the handle.
async fn run(
    url: String,
    events: mpsc::UnboundedSender<SocketEvent>,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
) {
    let mut held: Vec<Held> = Vec::new();
    // The subscribes sent over the wire and not yet answered, oldest first -
    // what a refusal from the core is attributed to.
    let mut awaiting: Vec<String> = Vec::new();
    let mut wait = RETRY_MS;
    say(&events, SocketEvent::Status(Status::Connecting));

    loop {
        // **The disconnected wait.** Outbound messages are still taken, so a
        // subscribe made while the socket is down is held locally and the
        // reconnect re-asks it; a close ends the task.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait);
        let stopped = loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => break false,
                next = outbound.recv() => {
                    let Some(next) = next else { return };
                    if apply_offline(next, &mut held, &mut awaiting) {
                        break true;
                    }
                }
            }
        };
        if stopped {
            say(&events, SocketEvent::Status(Status::Closed));
            return;
        }

        let ws = match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => ws,
            Err(_) => {
                wait = wait.saturating_mul(2).min(MAX_RETRY_MS);
                continue;
            }
        };
        say(&events, SocketEvent::Status(Status::Open));
        wait = RETRY_MS;
        let (mut writer, mut reader) = ws.split();
        // **The declaration is re-asked, once per held subscription**, with
        // the flags it carried: this is how the role survives a drop.
        for h in &held {
            if writer.send(h.subscribe_frame()).await.is_err() {
                break;
            }
            awaiting.push(h.key.clone());
        }
        let mut pending: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();
        let mut closing = false;
        loop {
            tokio::select! {
                frame = reader.next() => match frame {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(&text) {
                        Ok(value) => receive(&value, &mut pending, &mut awaiting, &events),
                        Err(why) => say(&events, SocketEvent::Malformed(why.to_string())),
                    },
                    Some(Ok(_)) => {}
                    Some(Err(why)) => {
                        say(&events, SocketEvent::Malformed(why.to_string()));
                        break;
                    }
                    None => break,
                },
                next = outbound.recv() => {
                    let Some(next) = next else { return };
                    match next {
                        Outbound::Close => {
                            closing = true;
                            break;
                        }
                        Outbound::Subscribe { what, answering, browser } => {
                            if let Some(key) = hold(&mut held, what, answering, browser) {
                                if let Some(h) = held.iter().find(|h| h.key == key) {
                                    let _ = writer.send(h.subscribe_frame()).await;
                                }
                                awaiting.push(key);
                            }
                        }
                        Outbound::Unsubscribe { what } => {
                            unhold(&mut held, &mut awaiting, &what);
                            let _ = writer
                                .send(Message::Text(json!({ "kind": "unsubscribe", "what": what }).to_string().into()))
                                .await;
                        }
                        Outbound::More { conversation, before, turns } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "more", "conversation": conversation, "before": before, "turns": turns })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                        Outbound::Devices => {
                            let _ = writer.send(Message::Text(json!({ "kind": "devices" }).to_string().into())).await;
                        }
                        Outbound::Dispatch { command, reply, sent, replied } => {
                            let ok = writer
                                .send(Message::Text(
                                    json!({ "kind": "command", "command": command, "reply_to": reply })
                                        .to_string()
                                        .into(),
                                ))
                                .await
                                .is_ok();
                            let _ = sent.send(if ok {
                                Ok(())
                            } else {
                                Err("the socket is open but the frame did not go".to_owned())
                            });
                            if ok
                                && let (Some(id), Some(tx)) = (reply, replied)
                            {
                                pending.insert(id, tx);
                            }
                        }
                        Outbound::Answer { id, parts, error } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "browser_answer", "id": id, "parts": parts, "error": error })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                        Outbound::TakeRole => {
                            let _ = writer.send(Message::Text(json!({ "kind": "browser_take_role" }).to_string().into())).await;
                        }
                        Outbound::Binary(bytes) => {
                            let _ = writer.send(Message::Binary(bytes.into())).await;
                        }
                    }
                }
            }
        }
        // **The connection left: whatever it was holding answers now**, the
        // role goes back, and the asks a drop killed are forgotten so a later
        // refusal finds its own.
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err("the socket dropped before answering".to_owned()));
        }
        awaiting.clear();
        say(&events, SocketEvent::Role { hosting: false });
        if closing {
            say(&events, SocketEvent::Status(Status::Closed));
            return;
        }
        say(&events, SocketEvent::Status(Status::Connecting));
        wait = wait.saturating_mul(2).min(MAX_RETRY_MS);
    }
}

fn say(events: &mpsc::UnboundedSender<SocketEvent>, event: SocketEvent) {
    let _ = events.send(event);
}

/// One server frame: replies are correlated here, a refusal goes to the
/// oldest subscribe still waiting, the role frame becomes an event, and
/// everything else crosses verbatim.
fn receive(
    value: &Value,
    pending: &mut HashMap<u64, oneshot::Sender<Result<Value, String>>>,
    awaiting: &mut Vec<String>,
    events: &mpsc::UnboundedSender<SocketEvent>,
) {
    match value.get("kind").and_then(Value::as_str) {
        Some("reply") => {
            if let Some(id) = value.get("reply_to").and_then(Value::as_u64)
                && let Some(tx) = pending.remove(&id)
            {
                let _ = tx.send(Ok(value.get("body").cloned().unwrap_or(Value::Null)));
            }
        }
        Some("browser_role") => {
            let hosting = value.get("hosting").and_then(Value::as_bool).unwrap_or(false);
            say(events, SocketEvent::Role { hosting });
        }
        Some("error") => {
            if value.get("what").and_then(Value::as_str) == Some("subscribe")
                && !awaiting.is_empty()
            {
                let key = awaiting.remove(0);
                let why = value.get("why").and_then(Value::as_str).unwrap_or_default().to_owned();
                say(events, SocketEvent::Refused { key, why });
                return;
            }
            say(events, SocketEvent::Message(value.clone()));
        }
        _ => say(events, SocketEvent::Message(value.clone())),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A stub forge: accepts connections, forwards every text frame it hears,
    /// and broadcasts whatever the test pushes. `kill` drops the live
    /// connections while the listener lives on, which is what a reconnect
    /// lands on.
    pub(crate) struct Stub {
        pub(crate) url: String,
        heard: mpsc::UnboundedReceiver<Value>,
        pub(crate) say: tokio::sync::broadcast::Sender<Value>,
        pub(crate) kill: tokio::sync::broadcast::Sender<()>,
        /// Kept so the broadcast always has a receiver: `send` refuses when
        /// nobody is subscribed, and a test may push before the socket's own
        /// per-connection task has joined.
        _keep: tokio::sync::broadcast::Receiver<Value>,
        /// The same, for the kill channel: a broadcast with no receiver
        /// refuses to send.
        _kill_keep: tokio::sync::broadcast::Receiver<()>,
    }

    pub(crate) async fn stub() -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let addr = listener.local_addr().expect("the bound address");
        let (heard_tx, heard) = mpsc::unbounded_channel();
        let (say, keep) = tokio::sync::broadcast::channel::<Value>(64);
        let (kill, kill_keep) = tokio::sync::broadcast::channel(8);
        let say_for_task = say.clone();
        let kill_for_task = kill.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let heard_tx = heard_tx.clone();
                let mut said = say_for_task.subscribe();
                let mut killed = kill_for_task.subscribe();
                tokio::spawn(async move {
                    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else { return };
                    let (mut writer, mut reader) = ws.split();
                    loop {
                        tokio::select! {
                            _ = killed.recv() => break,
                            frame = reader.next() => match frame {
                                Some(Ok(Message::Text(text))) => {
                                    if let Ok(value) = serde_json::from_str(&text) {
                                        let _ = heard_tx.send(value);
                                    }
                                }
                                _ => break,
                            },
                            pushed = said.recv() => match pushed {
                                Ok(value) => {
                                    let _ = writer
                                        .send(Message::Text(value.to_string().into()))
                                        .await;
                                }
                                Err(_) => break,
                            },
                        }
                    }
                });
            }
        });
        Stub {
            url: format!("ws://{addr}/socket"),
            heard,
            say,
            kill,
            _keep: keep,
            _kill_keep: kill_keep,
        }
    }

    impl Stub {
        pub(crate) async fn next_heard(&mut self) -> Value {
            tokio::time::timeout(Duration::from_secs(2), self.heard.recv())
                .await
                .expect("a frame within the bound")
                .expect("the stub still feeding")
        }

        /// Frames until the predicate is satisfied, so a test reads the one
        /// it cares about rather than the one that happened to arrive.
        pub(crate) async fn heard_until(&mut self, mut ok: impl FnMut(&Value) -> bool) -> Value {
            loop {
                let value = self.next_heard().await;
                if ok(&value) {
                    return value;
                }
            }
        }
    }

    /// A socket already past its open, with the event stream dropped: for
    /// tests that only care about what the wire hears.
    pub(crate) async fn open_connected(stub: &Stub) -> Socket {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let socket = Socket::open(stub.url.clone(), tx);
        loop {
            if let SocketEvent::Status(Status::Open) = next_event(&mut rx).await {
                return socket;
            }
        }
    }

    /// Open a socket against a fresh stub, with its event stream.
    fn opened(stub: &Stub) -> (Socket, mpsc::UnboundedReceiver<SocketEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Socket::open(stub.url.clone(), tx), rx)
    }

    /// Open a socket and wait until the connection is really up, so a test's
    /// first send is not folded into the disconnected wait.
    async fn connected(stub: &Stub) -> (Socket, mpsc::UnboundedReceiver<SocketEvent>) {
        let (socket, mut rx) = opened(stub);
        loop {
            if let SocketEvent::Status(Status::Open) = next_event(&mut rx).await {
                break;
            }
        }
        (socket, rx)
    }

    async fn next_event(rx: &mut mpsc::UnboundedReceiver<SocketEvent>) -> SocketEvent {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("an event within the bound")
            .expect("the socket still feeding")
    }

    /// The next event of one kind, skipping the others (statuses interleave
    /// freely with messages).
    async fn event_of(rx: &mut mpsc::UnboundedReceiver<SocketEvent>, kind: &str) -> SocketEvent {
        loop {
            let event = next_event(rx).await;
            if event.kind() == kind {
                return event;
            }
        }
    }

    /// The socket opens, says where it is, and carries a server frame through.
    #[tokio::test]
    async fn opens_reports_status_and_carries_frames() {
        let stub = stub().await;
        let (socket, mut rx) = opened(&stub);
        assert_eq!(next_event(&mut rx).await.kind(), "status");
        loop {
            if let SocketEvent::Status(Status::Open) = next_event(&mut rx).await {
                break;
            }
        }
        let greeting = json!({ "kind": "greeting", "version": 8, "settings": {} });
        stub.say.send(greeting.clone()).expect("the stub broadcasts");
        match event_of(&mut rx, "message").await {
            SocketEvent::Message(value) => assert_eq!(value, greeting),
            other => panic!("a message expected, got {other:?}"),
        }
        let _ = socket;
    }

    /// A subject subscribed BEFORE the socket opened is held locally and
    /// asked once the connection lands - `socket.ts`'s own case.
    #[tokio::test]
    async fn holds_a_subscribe_made_before_the_socket_opened() {
        let mut stub = stub().await;
        let (socket, _rx) = opened(&stub);
        socket.subscribe(json!("home"), true, true);
        let first = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(first["what"], json!("home"));
        assert_eq!(first["browser"], true);
    }

    /// The declaration rides every subscribe, and a refusal goes to the
    /// OLDEST subscribe still waiting - `socket.ts`'s `awaiting` queue.
    #[tokio::test]
    async fn declares_the_flags_it_was_given_and_attributes_a_refusal_to_the_oldest() {
        let mut stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        socket.subscribe(json!("home"), true, true);
        socket.subscribe(json!("usage"), true, false);
        let first = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(first["what"], json!("home"));
        assert_eq!(first["answering"], true);
        assert_eq!(first["browser"], true);
        let second = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(second["what"], json!("usage"));
        stub.say
            .send(json!({ "kind": "error", "what": "subscribe", "why": "no" }))
            .expect("broadcast");
        match event_of(&mut rx, "refused").await {
            SocketEvent::Refused { key, why } => {
                assert_eq!(key, "home");
                assert_eq!(why, "no");
            }
            other => panic!("a refusal expected, got {other:?}"),
        }
    }

    /// A seat subject keys as the TS side keys it, so held-dedup and
    /// refusals line up across the two implementations.
    #[test]
    fn a_seat_keys_by_its_slot() {
        let seat =
            json!({ "session": { "org": "Busytools", "project": "forge", "label": "lead" } });
        assert_eq!(subject_key(&seat), "session:Busytools\u{0}forge\u{0}lead");
        assert_eq!(subject_key(&json!("home")), "home");
    }

    /// A reply is handed to the caller that asked, by its own id.
    #[tokio::test]
    async fn hands_a_reply_to_the_caller_that_asked_by_its_own_id() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        let calling = tokio::spawn({
            let socket = socket.clone();
            async move { socket.dispatch(json!({ "prompt": {} }), true).await }
        });
        let sent = stub.heard_until(|v| v["kind"] == "command").await;
        let id = sent["reply_to"].as_u64().expect("a minted reply id");
        assert_eq!(id, 1, "the first reply id is 1, as socket.ts mints it");
        stub.say
            .send(json!({ "kind": "reply", "reply_to": id, "body": { "ok": true } }))
            .expect("broadcast");
        let body =
            calling.await.expect("the call task").expect("the call resolves").expect("a body");
        assert_eq!(body, json!({ "ok": true }));
    }

    /// A command sent over a closed connection is refused, not swallowed.
    #[tokio::test]
    async fn refuses_a_command_over_a_closed_connection() {
        let stub = stub().await;
        let (socket, mut rx) = opened(&stub);
        socket.close();
        loop {
            if let SocketEvent::Status(Status::Closed) = next_event(&mut rx).await {
                break;
            }
        }
        tokio::time::timeout(Duration::from_secs(2), socket.dispatch(json!({}), false))
            .await
            .expect("the refusal lands fast")
            .expect_err("a closed connection refuses");
    }

    /// What a command was holding is failed when the socket drops.
    #[tokio::test]
    async fn fails_what_a_command_was_holding_when_the_socket_drops() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        let calling = tokio::spawn({
            let socket = socket.clone();
            async move { socket.dispatch(json!({}), true).await }
        });
        let _ = stub.heard_until(|v| v["kind"] == "command").await;
        stub.kill.send(()).expect("the stub drops its connections");
        let why = tokio::time::timeout(Duration::from_secs(2), calling)
            .await
            .expect("the failure lands fast")
            .expect("the call task")
            .expect_err("the drop fails the call");
        assert!(why.contains("dropped"), "{why}");
    }

    /// A reconnect re-asks every held subscription once, with the flags it
    /// declared - the role's whole survival depends on this.
    #[tokio::test]
    async fn re_asks_every_held_subscription_after_a_drop() {
        let mut stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        socket.subscribe(json!("home"), true, true);
        let first = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(first["what"], json!("home"));
        // Drop the connection under the socket; the listener lives on, so the
        // reconnect lands and its re-ask is what this reads.
        stub.kill.send(()).expect("the stub drops its connections");
        // The drop must also release the role and say where the connection is.
        let mut saw_role = false;
        loop {
            match next_event(&mut rx).await {
                SocketEvent::Role { hosting: false } => saw_role = true,
                SocketEvent::Status(Status::Connecting) => break,
                _ => {}
            }
        }
        assert!(saw_role, "the role goes back with the drop");
        let again = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(again["what"], json!("home"));
        assert_eq!(again["browser"], true, "the declaration is carried, not defaulted");
        assert_eq!(again["answering"], true);
    }

    /// An unsubscribe drops its held entry: the reconnect that follows does
    /// not ask for it again.
    #[tokio::test]
    async fn an_unsubscribe_drops_one_held_entry() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        socket.subscribe(json!("home"), true, false);
        socket.subscribe(json!("usage"), false, false);
        let _ = stub.heard_until(|v| v["kind"] == "subscribe" && v["what"] == json!("home")).await;
        let _ = stub.heard_until(|v| v["kind"] == "subscribe" && v["what"] == json!("usage")).await;
        socket.unsubscribe(json!("home"));
        let _ = stub.heard_until(|v| v["kind"] == "unsubscribe").await;
        stub.kill.send(()).expect("the stub drops its connections");
        // The first frame the fresh connection hears is the surviving ask.
        let again = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(again["what"], json!("usage"));
    }
}
