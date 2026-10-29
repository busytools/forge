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

/// How many unsolicited events the input connection may fall behind by.
///
/// **A page reports whether anyone listens.** Events are drained only when a
/// call touches the page, so a page that keeps reporting between calls would
/// grow an unbounded queue for as long as nothing looks. Past this bound the
/// reader drops the newest event and keeps reading the socket, so replies
/// still arrive; a connection that is being drained never reaches it.
///
/// **Only the input connection takes this bound.** The navigation watch and
/// the passkey veto read every event, and a dropped `Target.attachedToTarget`
/// leaves that target paused for the host's life.
pub(crate) const EVENT_BACKLOG: usize = 256;

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
///
/// **Two shapes because a dropped event is only safe for the input
/// connection.** It drains and drops on every call, so a backlog ceiling is
/// growth insurance there; the navigation watch and the passkey veto read
/// every event, and the veto's `Target.attachedToTarget` is what releases a
/// paused target.
pub enum Events {
    Bounded(mpsc::Receiver<Value>),
    Unbounded(mpsc::UnboundedReceiver<Value>),
}

impl Events {
    pub async fn recv(&mut self) -> Option<Value> {
        match self {
            Self::Bounded(events) => events.recv().await,
            Self::Unbounded(events) => events.recv().await,
        }
    }

    /// Discard whatever the reader has delivered so far.
    pub fn drain(&mut self) {
        match self {
            Self::Bounded(events) => while events.try_recv().is_ok() {},
            Self::Unbounded(events) => while events.try_recv().is_ok() {},
        }
    }
}

enum EventSink {
    Bounded(mpsc::Sender<Value>),
    Unbounded(mpsc::UnboundedSender<Value>),
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
    /// connection and every event the endpoint reports.
    ///
    /// **Unbounded on purpose**: neither caller of this shape can afford a
    /// dropped event. See [`Self::connect_watch_bounded`] for the one that
    /// can.
    pub async fn connect_watch(port: u16, path: &str) -> Result<(Self, Events), String> {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        Self::connect(port, path, EventSink::Unbounded(events_tx), Events::Unbounded(events_rx))
            .await
    }

    /// Connect the same way, with a ceiling on how far the reader may fall
    /// behind before the newest event is dropped.
    ///
    /// The input connection's shape: it drains whatever arrived on each call
    /// and ignores it, so a lost event costs nothing.
    pub async fn connect_watch_bounded(
        port: u16,
        path: &str,
        backlog: usize,
    ) -> Result<(Self, Events), String> {
        let (events_tx, events_rx) = mpsc::channel(backlog);
        Self::connect(port, path, EventSink::Bounded(events_tx), Events::Bounded(events_rx)).await
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

    /// **The unbounded sink never drops**, which is the property the veto's
    /// `Target.attachedToTarget` depends on: one dropped release leaves that
    /// target paused for the host's life.
    #[test]
    fn an_unbounded_watcher_keeps_every_event() {
        let (events, mut watcher) = mpsc::unbounded_channel();
        let events = EventSink::Unbounded(events);

        for n in 0..(EVENT_BACKLOG + 1) {
            assert_eq!(forward_event(&events, json!({ "n": n })), Forwarded::Sent);
        }
        let mut kept = 0;
        while watcher.try_recv().is_ok() {
            kept += 1;
        }
        assert_eq!(kept, EVENT_BACKLOG + 1, "every event past the bounded ceiling is kept");
    }
}
