//! One connection to a browser host's devtools endpoint, speaking just enough
//! CDP for the surface's input: numbered calls, replies matched by id, and
//! the endpoint's events delivered on their own channel.
//!
//! **The read half runs on its own task.** Input sends without waiting for a
//! reply, but the page's own events - a file chooser opening, above all -
//! arrive while no call is pending, so somebody has to be reading the socket
//! between calls.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::stream::SplitSink;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type Replies = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// How many unsolicited events a connection may fall behind by, where its
/// backlog is bounded.
///
/// **A page reports whether anyone listens.** Events are drained only when a
/// call touches the page, so a page that keeps reporting between calls would
/// grow an unbounded queue for as long as nothing looks. Past this bound the
/// reader drops the newest event and keeps reading the socket, so replies
/// still arrive; a connection that is being drained never reaches it.
pub const EVENT_BACKLOG: usize = 256;

/// What a connection's watcher may fall behind by, and what it may not lose.
pub enum Backlog {
    /// Every event, with no ceiling. The navigation watch's shape: it reads
    /// every event and acts only on a history one, so its queue drains
    /// between the frames it does anything about.
    EveryEvent,
    /// The newest event past `ceiling` is dropped. The input connection's
    /// shape: it drains whatever arrived on each call and ignores it.
    DropNewest(usize),
    /// A ceiling on the events outside `methods`, while every event naming one
    /// of them is queued whatever the depth. The passkey veto's shape: its
    /// loop awaits a CDP round trip per target report, and a dropped target
    /// report leaves that target paused for the host's life.
    KeepNamed { methods: &'static [&'static str], ceiling: usize },
}

/// What became of one unsolicited event handed to the page's watcher.
#[derive(Debug, PartialEq, Eq)]
enum Forwarded {
    Sent,
    /// The watcher's backlog was full. The event is dropped and the reader
    /// keeps reading the socket.
    Dropped,
    /// The watcher is gone, so there is nobody to read the endpoint for.
    Closed,
}

/// The events one connection delivers, in the shape its reader needs.
pub enum Events {
    Bounded(mpsc::Receiver<Value>),
    Unbounded(mpsc::UnboundedReceiver<Value>),
    /// An unbounded queue whose non-`methods` events stop being queued past
    /// `ceiling`; `queued` is what the reader has not taken yet.
    Keeping {
        events: mpsc::UnboundedReceiver<Value>,
        queued: Arc<std::sync::atomic::AtomicUsize>,
    },
}

impl Events {
    pub async fn recv(&mut self) -> Option<Value> {
        match self {
            Self::Bounded(events) => events.recv().await,
            Self::Unbounded(events) => events.recv().await,
            Self::Keeping { events, queued } => {
                let event = events.recv().await;
                if event.is_some() {
                    queued.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
                event
            }
        }
    }

    /// Discard whatever the reader has delivered so far.
    pub fn drain(&mut self) {
        match self {
            Self::Bounded(events) => while events.try_recv().is_ok() {},
            Self::Unbounded(events) => while events.try_recv().is_ok() {},
            Self::Keeping { events, queued } => {
                while events.try_recv().is_ok() {
                    queued.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
}

enum EventSink {
    Bounded(mpsc::Sender<Value>),
    Unbounded(mpsc::UnboundedSender<Value>),
    /// An unbounded queue whose non-`methods` events are dropped past
    /// `ceiling`; the named ones are always queued.
    Keeping {
        events: mpsc::UnboundedSender<Value>,
        methods: &'static [&'static str],
        ceiling: usize,
        queued: Arc<std::sync::atomic::AtomicUsize>,
    },
}

fn forward_event(tx: &EventSink, message: Value) -> Forwarded {
    match tx {
        EventSink::Bounded(tx) => match tx.try_send(message) {
            Ok(()) => Forwarded::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => Forwarded::Dropped,
            Err(mpsc::error::TrySendError::Closed(_)) => Forwarded::Closed,
        },
        // An unbounded send has no full case; it fails only once the reader
        // has gone.
        EventSink::Unbounded(tx) => {
            tx.send(message).map_or(Forwarded::Closed, |()| Forwarded::Sent)
        }
        EventSink::Keeping { events, methods, ceiling, queued } => {
            let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
            if !methods.contains(&method)
                && queued.load(std::sync::atomic::Ordering::Relaxed) >= *ceiling
            {
                return Forwarded::Dropped;
            }
            match events.send(message) {
                Ok(()) => {
                    queued.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Forwarded::Sent
                }
                Err(_) => Forwarded::Closed,
            }
        }
    }
}

pub struct Cdp {
    sink: SplitSink<Socket, Message>,
    replies: Replies,
    next_id: u64,
    /// Raised when this handle is dropped, so the reader task ends instead of
    /// outliving the surface that opened it.
    closed: Arc<Notify>,
}

impl Drop for Cdp {
    fn drop(&mut self) {
        self.closed.notify_one();
    }
}

impl Cdp {
    /// Send one call without waiting for its reply.
    ///
    /// **Input is ordered by the socket, not by the replies.** Awaiting each
    /// event's answer puts a round trip in front of every keystroke and wheel
    /// tick, which is exactly what a page feels as lag; the reply to an
    /// `Input.*` call carries nothing a page needs.
    pub async fn send(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.next_id += 1;
        let message = json!({ "id": self.next_id, "method": method, "params": params });
        self.sink
            .send(Message::Text(message.to_string().into()))
            .await
            .map_err(|why| format!("{method} could not be sent: {why}"))
    }

    /// Send one call on `session` without waiting for its reply - the same
    /// one-way ordering `send` gives, addressed the way a flattened session
    /// needs.
    ///
    /// **A command whose reply is not the thing to wait for.** The veto's
    /// carried navigation must not hold its event loop: a target that
    /// attaches while the reply is pending - a service worker, above all -
    /// would keep waiting for a debugger nobody can release (measured
    /// 2026-10-23, #2534).
    pub async fn send_in(
        &mut self,
        method: &str,
        params: Value,
        session: &str,
    ) -> Result<(), String> {
        self.next_id += 1;
        let message =
            json!({ "id": self.next_id, "method": method, "params": params, "sessionId": session });
        self.sink
            .send(Message::Text(message.to_string().into()))
            .await
            .map_err(|why| format!("{method} could not be sent: {why}"))
    }

    /// Send one call and hand back the receiver its reply will arrive on, so
    /// a caller can watch a one-way command's outcome from a task of its own.
    ///
    /// **For work that must not hold an event loop and whose failure is
    /// still worth a line**: the veto's release and context patch are sent
    /// this way, and a refused one is named without stalling the loop that
    /// releases the next paused target (measured 2026-10-23, #2534).
    pub async fn send_watch(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<oneshot::Receiver<Value>, String> {
        self.next_id += 1;
        let id = self.next_id;
        let (waiter, answer) = oneshot::channel();
        self.replies.lock().unwrap_or_else(PoisonError::into_inner).insert(id, waiter);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.to_owned());
        }
        if let Err(why) = self.sink.send(Message::Text(message.to_string().into())).await {
            self.replies.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            return Err(format!("{method} could not be sent: {why}"));
        }
        Ok(answer)
    }

    /// Connect to the endpoint a host's page target names, answering the
    /// connection and the endpoint's events, with [`Backlog`] deciding what
    /// its reader may fall behind by and what it must not lose.
    pub async fn connect_watch(
        port: u16,
        path: &str,
        backlog: Backlog,
    ) -> Result<(Self, Events), String> {
        let (events_tx, events) = match backlog {
            Backlog::EveryEvent => {
                let (tx, rx) = mpsc::unbounded_channel();
                (EventSink::Unbounded(tx), Events::Unbounded(rx))
            }
            Backlog::DropNewest(ceiling) => {
                let (tx, rx) = mpsc::channel(ceiling);
                (EventSink::Bounded(tx), Events::Bounded(rx))
            }
            Backlog::KeepNamed { methods, ceiling } => {
                let (tx, rx) = mpsc::unbounded_channel();
                let queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                (
                    EventSink::Keeping {
                        events: tx,
                        methods,
                        ceiling,
                        queued: Arc::clone(&queued),
                    },
                    Events::Keeping { events: rx, queued },
                )
            }
        };
        Self::connect(port, path, events_tx, events).await
    }

    async fn connect(
        port: u16,
        path: &str,
        events_tx: EventSink,
        events: Events,
    ) -> Result<(Self, Events), String> {
        let url = format!("ws://127.0.0.1:{port}{path}");
        let (socket, _) = tokio_tungstenite::connect_async(&url).await.map_err(|why| {
            format!("the browser's devtools endpoint at {url} did not accept: {why}")
        })?;
        let (sink, mut source) = socket.split();
        let replies: Replies = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(Notify::new());
        let reader_replies = Arc::clone(&replies);
        let reader_closed = Arc::clone(&closed);
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    frame = source.next() => frame,
                    () = reader_closed.notified() => break,
                };
                let Some(Ok(Message::Text(text))) = frame else {
                    if frame.is_none() {
                        break;
                    }
                    continue;
                };
                let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
                match message.get("id").and_then(Value::as_u64) {
                    Some(id) => {
                        let waiter = reader_replies
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(&id);
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(message);
                        }
                    }
                    None => {
                        if matches!(forward_event(&events_tx, message), Forwarded::Closed) {
                            break;
                        }
                    }
                }
            }
            // The endpoint closed, or the surface did: every call still
            // waiting is answered as closed rather than left hanging.
            reader_replies.lock().unwrap_or_else(PoisonError::into_inner).clear();
        });
        Ok((Self { sink, replies, next_id: 0, closed }, events))
    }

    /// One call, answered: the reply's `result`, or the protocol error's own
    /// sentence. The reader task matches the id; events go to the watcher.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let (waiter, answer) = oneshot::channel();
        self.replies.lock().unwrap_or_else(PoisonError::into_inner).insert(id, waiter);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.to_owned());
        }
        if let Err(why) = self.sink.send(Message::Text(message.to_string().into())).await {
            self.replies.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            return Err(format!("{method} could not be sent: {why}"));
        }
        let Ok(reply) = answer.await else {
            return Err(format!("{method}: the devtools endpoint closed before it answered"));
        };
        match reply.get("error") {
            Some(error) => Err(format!(
                "{method}: {}",
                error.get("message").and_then(Value::as_str).unwrap_or("refused")
            )),
            None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_event_backlog_drops_the_newest_and_a_gone_watcher_stops_the_reader() {
        let (events, watcher) = mpsc::channel(1);
        let events = EventSink::Bounded(events);

        assert_eq!(forward_event(&events, json!({ "n": 1 })), Forwarded::Sent);
        // The watcher has not drained; the bound holds and the reader goes on.
        assert_eq!(forward_event(&events, json!({ "n": 2 })), Forwarded::Dropped);

        drop(watcher);
        // Nobody is left to read the endpoint for, so the reader stops.
        assert_eq!(forward_event(&events, json!({ "n": 3 })), Forwarded::Closed);
    }

    /// **The input connection's ceiling is 256 events.** The number is
    /// written out rather than read off [`EVENT_BACKLOG`]: the point of the
    /// pin is that the bound stays this small, and a test that resized
    /// itself with the constant would pass at a ceiling four times looser.
    /// A page that reports past it with no call in between loses the newest
    /// event, which is what keeps a drain-and-drop connection from growing
    /// without bound.
    #[test]
    fn the_bounded_watcher_drops_past_the_event_backlog() {
        let (events, _watcher) = mpsc::channel(EVENT_BACKLOG);
        let events = EventSink::Bounded(events);
        for n in 0..256 {
            assert_eq!(
                forward_event(&events, json!({ "n": n })),
                Forwarded::Sent,
                "every event up to the ceiling is queued",
            );
        }
        assert_eq!(
            forward_event(&events, json!({ "n": 256 })),
            Forwarded::Dropped,
            "the event past the ceiling is dropped, not queued",
        );
    }

    /// **The unbounded sink never drops**: every event past the bounded
    /// ceiling is still queued. This pins the sink's own shape; which caller
    /// takes it is the `Backlog` each one names at its `connect_watch` call.
    #[test]
    fn an_unbounded_watcher_keeps_every_event() {
        let (events, mut watcher) = mpsc::unbounded_channel();
        let events = EventSink::Unbounded(events);

        for n in 0..=EVENT_BACKLOG {
            assert_eq!(forward_event(&events, json!({ "n": n })), Forwarded::Sent);
        }
        let mut kept = 0;
        while watcher.try_recv().is_ok() {
            kept += 1;
        }
        assert_eq!(kept, EVENT_BACKLOG + 1, "every event past the bounded ceiling is kept");
    }

    /// **A keeping watcher never loses the events it acts on.** The veto's
    /// queue has a ceiling for the noise and none for the two methods it
    /// acts on: one dropped `Target.attachedToTarget` leaves that target
    /// paused for the host's life, and one dropped context report leaves an
    /// existing world unpatched.
    #[test]
    fn a_keeping_watcher_drops_the_noise_and_keeps_what_it_acts_on() {
        let (events, mut watcher) = mpsc::unbounded_channel();
        let events = EventSink::Keeping {
            events,
            methods: &["Target.attachedToTarget"],
            ceiling: 2,
            queued: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let attach = |n: u8| json!({ "method": "Target.attachedToTarget", "params": { "n": n } });
        let noise = |n: u8| json!({ "method": "Network.dataReceived", "params": { "n": n } });

        assert_eq!(forward_event(&events, attach(1)), Forwarded::Sent);
        assert_eq!(forward_event(&events, noise(1)), Forwarded::Sent);
        assert_eq!(
            forward_event(&events, noise(2)),
            Forwarded::Dropped,
            "the queue is at its ceiling for the events it does not act on",
        );
        assert_eq!(
            forward_event(&events, attach(2)),
            Forwarded::Sent,
            "and a target report is never the event dropped",
        );

        let mut methods = Vec::new();
        while let Ok(event) = watcher.try_recv() {
            methods.push(event["method"].as_str().unwrap_or_default().to_owned());
        }
        assert_eq!(
            methods,
            vec![
                "Target.attachedToTarget".to_owned(),
                "Network.dataReceived".to_owned(),
                "Target.attachedToTarget".to_owned(),
            ],
            "both target reports arrive, in the order the endpoint sent them",
        );
    }
}
