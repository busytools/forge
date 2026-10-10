//! The webview's side of the connection: its commands in, its events out.
//!
//! **This is the seam between the two halves.** The socket and the records
//! are this process's (`socket.rs`, `records.rs`), and the page reaches them
//! only through the commands here - so nothing the page does, or fails to do,
//! is required for the socket to be read, an ask to be answered, or a
//! reconnect to run. That is the whole of the 2026-10-10 fix: a parked
//! webview starved every ask because the page was on the ask path, and it is
//! not any more.
//!
//! **Every frame still reaches the page.** Rule 25: the default is to hand
//! the frame on as itself, including a kind this build has never seen - a
//! page drawing it plainly is the mechanism working, and a filtered frame is
//! a drop. Three kinds are consumed here alone, each provably unusable by the
//! page: `reply` is `dispatch`'s own correlated answer (minted and matched in
//! `socket.rs`), `browser_role` crosses as its own event and its consumer is
//! `onBrowserRole`, and `browser_ask` MUST not reach the page on the desktop,
//! where no handler is registered for one - an ask answered from the page is
//! the defect this move removes.
//!
//! **Frames pause while the page is away, and catch up from the records.**
//! The page heartbeats; one older than `FRESH_MS` means a suspended webview,
//! and frames stop being emitted (they still fold into the records) so a
//! parked page cannot grow an unbounded event backlog. A heartbeat arriving
//! after a spell is answered with a reconcile, built from the held records
//! and delivered in the message kinds the page already reads: a snapshot per
//! held subject, which is what the record is, and per seat the newest page
//! its conversation makes - the answer that seat's `more` would have carried
//! - so the catch-up is complete rather than merely live again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::browser::BrowserHost;
use crate::records::{self, Records};
use crate::socket::{Socket, SocketEvent, Status};

/// How long a page's heartbeat stays fresh. Past it the webview is presumed
/// parked - a suspended page's timers stop, so its heartbeat stops with them -
/// and raw frames are counted rather than emitted. Bounds the backlog a park
/// can leave in flight, and is what the next heartbeat's reconcile answers.
const FRESH_MS: u64 = 15_000;

/// How many frames a parked spell may hold for the page. Only frames the
/// records do NOT rebuild are kept (the chat's own bulk is, in the record's
/// conversation), and those are human-paced - a seat swap, a dock's ending,
/// a dictation's close - so the ring is reached only by a park of hours. On
/// the pathological overflow the OLDEST go, loudly: the newest are the ones
/// whose effects a page would still be meeting.
const PARKED_RING: usize = 1024;

/// One fact for the page, as the app half emits it.
///
/// **The page's model is unchanged**: frames arrive as `Inbound` exactly as
/// they did over the webview's own WebSocket, so every store and surface
/// draws from the same stream it always did. The two other arms are the
/// connection's own facts, which never rode the frame stream: where it is in
/// its life, and who holds the browser role.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// The connection's state moved; the page re-reads it with `client_state`.
    Connection,
    /// A `ServerMessage`, as itself.
    Inbound(Value),
    /// A subscribe was refused, with the core's own words.
    Refused { key: String, why: String },
    /// The browser role moved.
    Role { hosting: bool },
    /// How many asks are in flight right now, for the strip's ring.
    Asks { inflight: usize },
}

/// One subject the page watches: its own subject, how many page readers hold
/// it, and the declaration they asked with.
struct Watch {
    what: Value,
    readers: usize,
    answering: bool,
    browser: bool,
}

struct State {
    records: Records,
    held: HashMap<String, Watch>,
    socket: Option<Socket>,
    status: &'static str,
    greeting: Option<Value>,
    role: bool,
    /// When the page last heartbeated, and how many frames went unforwarded
    /// while it was presumed parked. **The suppressed frames the records do
    /// not rebuild are HELD, not dropped** (rule 25): the ring carries them
    /// across the spell and the reconcile flushes it first, so a released
    /// seat, a dock's ending or a dictation's close still reaches the page.
    last_beat: Option<Instant>,
    suppressed: u64,
    parked: VecDeque<Value>,
    /// Seats a REPLACES frame has asked a fresh read for and no snapshot has
    /// answered yet: a second frame while one is out coalesces into it
    /// rather than asking again.
    refreshing: HashSet<String>,
}

/// The app half, as the Tauri layer drives it.
pub struct Bridge {
    state: Mutex<State>,
    host: Option<Arc<BrowserHost>>,
    events: mpsc::UnboundedSender<ClientEvent>,
    inflight: Arc<AtomicUsize>,
}

impl Bridge {
    pub fn new(
        host: Option<Arc<BrowserHost>>,
        events: mpsc::UnboundedSender<ClientEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                records: Records::new(),
                held: HashMap::new(),
                socket: None,
                status: "connecting",
                greeting: None,
                role: false,
                last_beat: None,
                suppressed: 0,
                parked: VecDeque::new(),
                refreshing: HashSet::new(),
            }),
            host,
            events,
            inflight: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: ClientEvent) {
        let _ = self.events.send(event);
    }

    /// Dial the server, replacing any connection this bridge had. Whatever
    /// the page already watches is re-asked on the new socket, so a
    /// reconnect from the connect screen lands the subscriptions it had.
    pub fn connect(self: &Arc<Self>, url: String) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let socket = Socket::open(url, tx);
        let wanted: Vec<(Value, bool, bool, usize)> = {
            let mut state = self.lock();
            state.socket = Some(socket.clone());
            state.status = "connecting";
            state.greeting = None;
            state.last_beat = Some(Instant::now());
            state.suppressed = 0;
            state.parked.clear();
            state.refreshing.clear();
            state
                .held
                .values()
                .map(|watch| (watch.what.clone(), watch.answering, watch.browser, watch.readers))
                .collect()
        };
        for (what, answering, browser, readers) in wanted {
            // Once per OPEN subscription: the server counts, and the new
            // socket starts at zero - a subject two page readers hold has to
            // be asked for twice, or the first unsubscribe silences it.
            for _ in 0..readers {
                socket.subscribe(what.clone(), answering, browser);
            }
        }
        self.emit(ClientEvent::Connection);
        let bridge = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                bridge.socket_event(event);
            }
        });
    }

    /// Watch one subject, as one page reader more. The wire frame is passed
    /// through per call - the server counts subscriptions, so two readers are
    /// two frames and the record's lifetime is this side's own count.
    pub fn subscribe(&self, what: Value, answering: bool, browser: bool) {
        let key = records::subject_key(&what);
        if key.is_empty() {
            return;
        }
        let mut state = self.lock();
        match state.held.get_mut(&key) {
            Some(watch) => {
                watch.readers += 1;
                watch.answering |= answering;
                watch.browser |= browser;
            }
            None => {
                state
                    .held
                    .insert(key, Watch { what: what.clone(), readers: 1, answering, browser });
            }
        }
        if let Some(socket) = &state.socket {
            socket.subscribe(what, answering, browser);
        }
    }

    /// Give one reader's subscription back. The record goes when the last
    /// page reader does, the way the page's own store does on leave.
    pub fn unsubscribe(&self, what: &Value) {
        let key = records::subject_key(what);
        let mut state = self.lock();
        let last = match state.held.get_mut(&key) {
            Some(watch) => {
                watch.readers = watch.readers.saturating_sub(1);
                watch.readers == 0
            }
            // Nothing was held: the frame still goes, because the wire is the
            // server's count and this side does not own it.
            None => false,
        };
        if last {
            state.held.remove(&key);
            state.records.release(&key);
        }
        if let Some(socket) = &state.socket {
            socket.unsubscribe(what.clone());
        }
    }

    /// Ask again for a subject the page already holds: the core answers with
    /// a snapshot, which replaces the record whole. **The declaration is the
    /// held one**: a refresh with flags of its own would escalate a seat
    /// watched without them, and the core parks turns on whatever answered.
    pub fn refresh(&self, what: &Value) {
        let state = self.lock();
        let key = records::subject_key(what);
        let (answering, browser) = state
            .held
            .get(&key)
            .map(|watch| (watch.answering, watch.browser))
            .unwrap_or((false, false));
        if let Some(socket) = &state.socket {
            records::refresh(socket, what, answering, browser);
        }
    }

    /// What the page reads at the start and on every `Connection` notice.
    pub fn state(&self) -> Value {
        let state = self.lock();
        json!({
            "status": state.status,
            "greeting": state.greeting,
            "role": state.role,
        })
    }

    /// Older turns of a conversation. False means no socket is open and no
    /// page is coming; the answer lands in the record's own conversation.
    pub fn more(&self, conversation: &Value, before: Option<String>, turns: u64) -> bool {
        let state = self.lock();
        if state.status != "open" {
            return false;
        }
        if let Some(socket) = &state.socket {
            socket.more(conversation.clone(), before, turns);
        }
        true
    }

    /// One dictation frame. False means the socket is not open, and the take
    /// holding that frame keeps it rather than losing it.
    pub fn frame(&self, bytes: Vec<u8>) -> bool {
        let state = self.lock();
        if state.status != "open" {
            return false;
        }
        if let Some(socket) = &state.socket {
            socket.send_binary(bytes);
        }
        true
    }

    /// Ask for the inputs forge can record from.
    pub fn devices(&self) -> bool {
        let state = self.lock();
        if state.status != "open" {
            return false;
        }
        if let Some(socket) = &state.socket {
            socket.devices();
        }
        true
    }

    /// Take the browser role from whoever holds it.
    pub fn take_role(&self) {
        let state = self.lock();
        let Some(socket) = &state.socket else { return };
        // The claim re-declares first (the page's own `declare`): the relay
        // registers only connections that declared, and one displaced by an
        // earlier take is no longer in its line - so the bare claim would
        // come back false with nothing re-registered.
        if let Some(watch) = state.held.values().next() {
            socket.subscribe(watch.what.clone(), watch.answering, true);
        }
        socket.take_role();
    }

    /// Send one core command. `reply` is the shim's own decision (the four
    /// commands that answer through a reply); Err means it was not sent.
    pub async fn dispatch(&self, command: Value, reply: bool) -> Result<Option<Value>, String> {
        let socket =
            self.lock().socket.clone().ok_or_else(|| "the socket is not open".to_owned())?;
        socket.dispatch(command, reply).await
    }

    /// The page's pulse, and where a parked spell is answered. One heartbeat
    /// older than `FRESH_MS` past its predecessor means the page did not run
    /// for that long - **whatever was counted**. A spell with no frames at
    /// all still needs its reconcile (the webview may have dropped clauses
    /// this side cannot see), and a copy of the records is cheap.
    pub fn heartbeat(&self) {
        let stale = {
            let mut state = self.lock();
            let stale =
                state.last_beat.is_some_and(|at| at.elapsed() >= Duration::from_millis(FRESH_MS));
            state.last_beat = Some(Instant::now());
            state.suppressed = 0;
            stale
        };
        if stale {
            self.reconcile();
        }
    }

    /// The page is behind: hand it what the park would have handed it.
    ///
    /// **The stream-only frames go first, as themselves**, in the order they
    /// were held: a released seat, a dock's ending, a dictation's close are
    /// frames the records do not rebuild, and a reconcile without them would
    /// be the drop rule 25 forbids.
    ///
    /// Then the records, in the shapes the page already reads them: a seat's
    /// record IS what the core's snapshot carries, so it goes back out as
    /// that snapshot - its conversation riding the newest page its `more`
    /// with no `before` would have carried, marked so the page can tell it
    /// from an older-page answer. A subject that is not a seat is ASKED FOR
    /// ANEW instead: its record folds nothing on this side (the page's own
    /// home fold reads the frames), so a copy of it here would be stale,
    /// while a re-subscribe answers with the truth.
    fn reconcile(&self) {
        let mut flush = Vec::new();
        {
            let mut state = self.lock();
            flush.extend(state.parked.drain(..));
        }
        for message in flush {
            self.emit(ClientEvent::Inbound(message));
        }
        let state = self.lock();
        for (key, watch) in &state.held {
            let Some(session) = watch.what.get("session") else {
                if let Some(socket) = &state.socket {
                    records::refresh(socket, &watch.what, watch.answering, watch.browser);
                }
                continue;
            };
            let Some(Ok(record)) = state.records.read(key) else { continue };
            self.emit(ClientEvent::Inbound(json!({
                "kind": "snapshot",
                "subject": watch.what,
                "data": record,
            })));
            let Some(turns) = record.get("conversation").and_then(|c| c.get("turns")) else {
                continue;
            };
            self.emit(ClientEvent::Inbound(json!({
                "kind": "page",
                "conversation": session,
                // No cursor: this is the newest window, the answer `more`
                // with no `before` would have carried.
                "cursor": null,
                "turns": turns,
                // **Told apart from an answer**: a page read while an older
                // ask is "in flight" would be spliced as an older page and
                // scramble the order, so the page is marked as the catch-up
                // it is.
                "reconciled": true,
            })));
        }
    }

    fn socket_event(self: &Arc<Self>, event: SocketEvent) {
        match event {
            SocketEvent::Status(status) => self.status_moved(status),
            SocketEvent::Message(message) => self.message(message),
            SocketEvent::Role { hosting } => {
                self.lock().role = hosting;
                self.emit(ClientEvent::Role { hosting });
            }
            SocketEvent::Refused { key, why } => {
                self.lock().records.refuse(&key, &why);
                self.emit(ClientEvent::Refused { key, why });
            }
            SocketEvent::Malformed(text) => {
                tauri_plugin_log::log::warn!("client socket: {text}");
            }
        }
    }

    fn status_moved(&self, status: Status) {
        {
            let mut state = self.lock();
            state.status = match status {
                Status::Connecting => "connecting",
                Status::Open => "open",
                Status::Closed => "closed",
            };
            if status == Status::Open {
                // A fresh connection's frames are forwarded at once: the page
                // is there (it just asked to connect), and the first
                // heartbeat carries the proof rather than the promise.
                state.last_beat = Some(Instant::now());
                state.suppressed = 0;
                state.parked.clear();
                state.refreshing.clear();
            } else {
                // A take is the page's own and its own status arm clears it
                // (#1880); everything else stands until the reconnect's
                // snapshots replace it.
                state.records.dropped();
            }
        }
        self.emit(ClientEvent::Connection);
    }

    fn message(self: &Arc<Self>, message: Value) {
        let kind = message.get("kind").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "greeting" => {
                self.lock().greeting = Some(message.clone());
                self.emit(ClientEvent::Connection);
                self.forward(&message);
            }
            "snapshot" => {
                let Some(subject) = message.get("subject") else { return };
                let data = message.get("data").cloned().unwrap_or(Value::Null);
                let key = records::subject_key(subject);
                let mut state = self.lock();
                state.refreshing.remove(&key);
                state.records.snapshot(subject, data);
                drop(state);
                self.forward(&message);
            }
            "update" => {
                let Some(update) = message.get("update") else { return };
                let refresh = self.lock().records.apply(update);
                self.forward(&message);
                self.acted(refresh);
            }
            "browser_ask" => self.answer_ask(&message),
            // Consumed here alone, each provably unusable by the page: a
            // reply is `dispatch`'s own answer, correlated by id in
            // `socket.rs`; the role's consumer is `onBrowserRole`, fed by the
            // event above; and an ask answered by the page is the defect this
            // move removes - the desktop page registers no handler for one.
            "reply" | "browser_role" => {}
            _ => self.forward(&message),
        }
    }

    /// Hand the page one frame, unless it is presumed parked.
    ///
    /// A parked spell's frames are not lost wholesale: the ones the records
    /// rebuild (a seat's chat, in the record's conversation) are only
    /// counted, and every other frame is HELD in the ring for the reconcile
    /// to flush - a released seat, a dock's ending, a dictation's close, an
    /// error, a devices answer are frames no record carries.
    fn forward(&self, message: &Value) {
        let mut state = self.lock();
        let fresh =
            state.last_beat.is_some_and(|at| at.elapsed() < Duration::from_millis(FRESH_MS));
        if !fresh {
            state.suppressed += 1;
            if !rebuilt_by_records(message) {
                if state.parked.len() >= PARKED_RING {
                    state.parked.pop_front();
                    tauri_plugin_log::log::warn!(
                        "a parked page's frame ring overflowed; the oldest went"
                    );
                }
                state.parked.push_back(message.clone());
            }
            return;
        }
        drop(state);
        self.emit(ClientEvent::Inbound(message.clone()));
    }

    /// Answer one ask here, on its own task: the page is not consulted, and
    /// the ring counts it so a strip can draw what is running.
    fn answer_ask(&self, frame: &Value) {
        let Some(socket) = self.lock().socket.clone() else { return };
        let Some(host) = self.host.clone() else {
            if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                socket.answer(id, json!([]), Some("this client has no browser host".to_owned()));
            }
            return;
        };
        let began = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.emit(ClientEvent::Asks { inflight: began });
        let (events, inflight) = (self.events.clone(), Arc::clone(&self.inflight));
        let frame = frame.clone();
        tokio::spawn(async move {
            crate::asks::run(&frame, &socket, &host).await;
            let left = inflight.fetch_sub(1, Ordering::SeqCst) - 1;
            let _ = events.send(ClientEvent::Asks { inflight: left });
        });
    }

    /// Do what a record fold asked of this half: re-read what a REPLACES frame
    /// took a new occupant for. The page is told nothing here - the frames
    /// are its notices, exactly as they were when it held the socket.
    fn acted(&self, refresh: Vec<String>) {
        if refresh.is_empty() {
            return;
        }
        let mut state = self.lock();
        for key in refresh {
            // A read already out for this seat answers everything a second
            // ask would (the page's own store coalesces the same way).
            if !state.refreshing.insert(key.clone()) {
                continue;
            }
            let Some(watch) = state.held.get(&key) else { continue };
            let (what, answering, browser) = (watch.what.clone(), watch.answering, watch.browser);
            if let Some(socket) = &state.socket {
                records::refresh(socket, &what, answering, browser);
            }
        }
    }

    /// End the connection for good, as the page's `close` does: no retry
    /// follows.
    pub fn close(&self) {
        let state = self.lock();
        if let Some(socket) = &state.socket {
            socket.close();
        }
    }
}

/// Whether the records rebuild this frame for a returning page: the chat's
/// own bulk, which the record's conversation carries. **One case wide on
/// purpose** - everything else, a kind neither side knows included, is held
/// across a park rather than dropped.
fn rebuilt_by_records(message: &Value) -> bool {
    message.get("kind").and_then(Value::as_str) == Some("update")
        && message
            .get("update")
            .is_some_and(|update| records::variant_name(update) == Some("chat_appended"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::tests::stub;

    fn bridge() -> (Arc<Bridge>, mpsc::UnboundedReceiver<ClientEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Bridge::new(None, tx), rx)
    }

    /// Await the next event of a kind, skipping others.
    async fn event(
        rx: &mut mpsc::UnboundedReceiver<ClientEvent>,
        want: impl Fn(&ClientEvent) -> bool,
    ) -> ClientEvent {
        loop {
            let event = rx.recv().await.expect("an event");
            if want(&event) {
                return event;
            }
        }
    }

    fn is_inbound() -> impl Fn(&ClientEvent) -> bool {
        |event| matches!(event, ClientEvent::Inbound(_))
    }

    fn is_inbound_kind(kind: &'static str) -> impl Fn(&ClientEvent) -> bool {
        move |event| matches!(event, ClientEvent::Inbound(message) if message["kind"] == json!(kind))
    }

    fn seat() -> Value {
        json!({ "session": { "org": "Busytools", "project": "forge", "label": "lead" } })
    }

    /// One frame in, driven where it lands rather than through the stub: the
    /// socket's own receive loop is `socket.rs`'s tested ground, and a test
    /// here is about what the bridge does with a frame.
    fn sent(bridge: &Arc<Bridge>, message: Value) {
        bridge.socket_event(SocketEvent::Message(message));
    }

    /// Park the page: nothing has heartbeated for longer than `FRESH_MS`.
    fn park(bridge: &Arc<Bridge>) {
        bridge.lock().last_beat = Some(Instant::now() - Duration::from_millis(FRESH_MS + 1_000));
    }

    /// Wait until the socket is up, so a stub push lands on a connection that
    /// is really there - the broadcast does not replay.
    async fn connected(bridge: &Arc<Bridge>) {
        for _ in 0..300 {
            if bridge.lock().status == "open" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the socket never opened");
    }

    fn snapshot(data: Value) -> Value {
        json!({ "kind": "snapshot", "subject": seat(), "data": data })
    }

    fn context_usage(percentage: f64) -> Value {
        json!({
            "kind": "update",
            "update": { "context_usage_snapshot": { "percentage": percentage, "key": seat()["session"] } },
        })
    }

    /// Every frame reaches the page as itself, an unknown kind included
    /// (rule 25) - and the record takes it as well.
    #[tokio::test]
    async fn a_frame_reaches_the_page_as_itself() {
        let (bridge, mut rx) = bridge();
        let server = stub().await;
        bridge.connect(server.url.clone());
        sent(&bridge, json!({ "kind": "greeting", "version": 8 }));
        sent(&bridge, snapshot(json!({ "header": { "turn_in_flight": false } })));
        let caught = event(&mut rx, is_inbound_kind("snapshot")).await;
        assert!(matches!(caught, ClientEvent::Inbound(_)));
        sent(&bridge, json!({ "kind": "something_new", "payload": { "a": 1 } }));
        let ClientEvent::Inbound(seen) = event(&mut rx, is_inbound()).await else {
            unreachable!("filtered to inbound");
        };
        assert_eq!(seen["kind"], "something_new");
        assert_eq!(seen["payload"]["a"], json!(1));
    }

    /// A parked page's frames stop being emitted (the records still take
    /// them), and the first heartbeat after the spell answers with the
    /// catch-up: the held record as its own snapshot, and the newest page its
    /// conversation makes.
    #[tokio::test]
    async fn a_parked_spell_reconciles_from_the_records() {
        let (bridge, mut rx) = bridge();
        let server = stub().await;
        bridge.connect(server.url.clone());
        bridge.subscribe(seat(), true, true);
        sent(
            &bridge,
            snapshot(json!({
                "header": { "context": { "percent": 0.1 } },
                "conversation": { "turns": [{ "key": "t1", "messages": [] }] },
            })),
        );
        event(&mut rx, is_inbound()).await;
        // The page goes away: its heartbeat stops, and the next frame is
        // counted rather than emitted - but the record still takes it.
        park(&bridge);
        sent(&bridge, context_usage(0.5));
        assert_eq!(bridge.lock().suppressed, 1, "the frame was counted, not forwarded");
        assert!(rx.try_recv().is_err(), "nothing was emitted while the page was away");
        // The page comes back: the heartbeat hands the park's stream-only
        // frames over first, then the records.
        bridge.heartbeat();
        let caught = event(&mut rx, is_inbound_kind("snapshot")).await;
        let ClientEvent::Inbound(caught) = caught else { unreachable!("filtered to inbound") };
        assert_eq!(caught["subject"], seat(), "the record goes back under its own subject");
        assert_eq!(
            caught["data"]["header"]["context"]["percent"],
            json!(0.5),
            "the record folded the frame the page missed"
        );
        let ClientEvent::Inbound(page) = event(&mut rx, is_inbound_kind("page")).await else {
            unreachable!("filtered to inbound");
        };
        assert_eq!(
            page["cursor"],
            json!(null),
            "the newest window, as `more` with no before answers"
        );
        assert_eq!(page["reconciled"], json!(true), "and it says which kind of page it is");
        assert_eq!(page["turns"][0]["key"], json!("t1"));
        assert_eq!(page["conversation"], seat()["session"]);
        assert_eq!(bridge.lock().suppressed, 0, "the spell is spent");
    }

    /// **The stream-only set crosses a park.** A released seat and a dock's
    /// ending are frames no record rebuilds, so the park holds them and the
    /// reconcile flushes them first; the chat's own bulk is rebuilt from the
    /// record and is NOT replayed, because a record behind it already
    /// carries it.
    #[tokio::test]
    async fn a_parked_spell_holds_the_frames_no_record_rebuilds() {
        let (bridge, mut rx) = bridge();
        let server = stub().await;
        bridge.connect(server.url.clone());
        bridge.subscribe(seat(), true, true);
        sent(&bridge, snapshot(json!({ "conversation": { "turns": [] } })));
        event(&mut rx, is_inbound()).await;
        park(&bridge);
        let releasing = json!({
            "kind": "update",
            "update": { "releasing": { "key": seat()["session"] } },
        });
        let chat = json!({
            "kind": "update",
            "update": { "chat_appended": { "msg": { "type": "user" }, "key": seat()["session"] } },
        });
        sent(&bridge, releasing.clone());
        sent(&bridge, chat);
        assert_eq!(bridge.lock().parked.len(), 1, "the chat is rebuilt; only the release is held");
        bridge.heartbeat();
        let ClientEvent::Inbound(first) = event(&mut rx, is_inbound()).await else {
            unreachable!("filtered to inbound");
        };
        assert_eq!(
            first["update"], releasing["update"],
            "the held frame goes out first, as itself"
        );
    }

    /// A second reader is a second wire frame, and the record lives until the
    /// last one lets go - observed through what a reconcile hands back.
    #[tokio::test]
    async fn two_readers_hold_one_record_until_the_last_lets_go() {
        let (bridge, mut rx) = bridge();
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(seat(), true, true);
        bridge.subscribe(seat(), true, true);
        sent(&bridge, snapshot(json!({ "a": 1 })));
        event(&mut rx, is_inbound()).await;
        // Both readers reached the wire as their own subscribe.
        server.heard_until(|v| v["kind"] == "subscribe").await;
        server.heard_until(|v| v["kind"] == "subscribe").await;
        bridge.unsubscribe(&seat());
        park(&bridge);
        sent(&bridge, context_usage(0.2));
        bridge.heartbeat();
        let ClientEvent::Inbound(caught) = event(&mut rx, is_inbound_kind("snapshot")).await else {
            unreachable!("filtered to inbound");
        };
        assert_eq!(caught["kind"], "snapshot", "a reader is still there, so the record is");
        // The last reader lets go: the record goes with it, and the
        // reconcile hands back no copy for a seat this client no longer
        // holds (the parked frame still replays as itself; no store holds it).
        bridge.unsubscribe(&seat());
        assert!(
            bridge.lock().records.read("session:Busytools\u{0}forge\u{0}lead").is_none(),
            "the last reader released the record"
        );
        park(&bridge);
        sent(&bridge, context_usage(0.3));
        bridge.heartbeat();
        while let Ok(event) = rx.try_recv() {
            if let ClientEvent::Inbound(message) = event {
                assert_ne!(
                    message["kind"],
                    json!("snapshot"),
                    "no record copy comes back for a released seat"
                );
            }
        }
    }

    /// A refused subscribe reaches the page with the core's own words, under
    /// the key the shim's store is held at.
    #[tokio::test]
    async fn a_refused_subscribe_reaches_the_page() {
        let (bridge, mut rx) = bridge();
        let key = "session:Busytools\u{0}forge\u{0}lead";
        bridge.socket_event(SocketEvent::Refused {
            key: key.to_owned(),
            why: "no session there".to_owned(),
        });
        let ClientEvent::Refused { key: seen, why } = event(&mut rx, |_| true).await else {
            unreachable!("the first event is the refusal");
        };
        assert_eq!(seen, key);
        assert_eq!(why, "no session there");
        assert!(
            matches!(bridge.lock().records.read(key), Some(Err("no session there"))),
            "the record holds the refusal for the reconcile too"
        );
    }

    /// An ask is answered by this process, counted for the ring, and the page
    /// hears nothing of it.
    #[tokio::test]
    async fn an_ask_is_answered_here_and_counted_for_the_ring() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host = Arc::new(BrowserHost::unavailable("no stack in the test".to_owned()));
        let bridge = Bridge::new(Some(host), tx);
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        let ask = json!({
            "kind": "browser_ask",
            "id": 3,
            "seat": { "org": "Busytools", "project": "forge", "label": "lead" },
            "tool": "browser_navigate",
            "args": {},
        });
        server.say.send(ask).expect("an ask");
        let answer = server.heard_until(|v| v["kind"] == "browser_answer").await;
        assert_eq!(answer["id"], 3);
        assert_eq!(answer["error"], "no stack in the test");
        // The ring saw the ask begin and end.
        let mut saw = vec![];
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(500), async { rx.recv().await }).await
        {
            if let ClientEvent::Asks { inflight } = event {
                saw.push(inflight);
            }
            if saw.len() == 2 {
                break;
            }
        }
        assert_eq!(saw, vec![1, 0], "one ask in flight, then none");
    }
}
