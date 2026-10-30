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
//! semantics are the TS tests': a reconnect declares every held subject
//! again with the role flags it carried - **except a seat, which is the
//! page's own to re-open** and re-attaches on its next click - a refusal is
//! attributed to the oldest subscribe still waiting, and a drop fails
//! whatever a command was holding. The web client at forge.hub has no
//! Tauri process behind it and keeps `socket.ts`; only the desktop selects
//! this half.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// The first reconnect wait; it doubles up to the ceiling and resets on an
/// open, exactly as `socket.ts`'s `retryDelay` does.
const RETRY_MS: u64 = 50;
const MAX_RETRY_MS: u64 = 2000;

/// How long the link may carry no bytes before this half asks it a question.
const LIVENESS_EVERY: Duration = Duration::from_secs(15);

/// How long a probe may go unanswered by a BYTE before the link is called
/// dead.
///
/// **A half-open socket gives no other evidence**: the peer's close never
/// arrives, writes buffer, and the status stays `open` for ever while the
/// page draws pre-drop records. Any inbound byte answers a probe, so this
/// only expires on a link that carries nothing in either direction - and
/// closing here runs the existing drop path, retry included.
///
/// **Bytes, not messages.** tungstenite hands a frame over only once it is
/// complete, and the server may spend its whole write bound (60 s,
/// `crates/forge-server/src/transport.rs`) on one; a message-level deadline
/// would call a live peer dead mid-frame, redial, and re-ask the same record -
/// a permanent reconnect loop on exactly the poor link this is for.
const LIVENESS_DEAD: Duration = Duration::from_secs(10);

/// The bound one dial is given. A peer that accepts TCP and never completes
/// the handshake would otherwise strand the retry loop on a dial that never
/// returns.
const DIAL_BOUND: Duration = Duration::from_secs(10);

/// The protocol that added `ping` and `pong`.
///
/// **An older server answers a probe with `client_message`**, and this half
/// books a `client_message` refusal on the oldest subscribe still waiting - so
/// a probe sent to a skewed server would mark a healthy subject refused. The
/// greeting says which server this is, so a pre-liveness one is never asked.
const LIVENESS_PROTOCOL: u64 = 20;

/// The bound one reply-carrying dispatch is given. The socket is loopback and
/// its server answers in milliseconds; a reply that outlives this is the
/// version-skew case an `error` frame answers without a reply id, or a wedge -
/// and a caller parked on it would never return. **A despawn is exempt**: its
/// server path removes a worktree and reaps a branch first, which can run for
/// minutes (#1922, #1113), so no finite bound is honest.
const REPLY_BOUND: Duration = Duration::from_secs(10);

/// The bound an attach gets: the server starts a host and its driver before
/// it answers, which a cold machine measured at about 14 seconds, so this
/// sits above that rather than above a socket round trip.
const ATTACH_BOUND: Duration = Duration::from_secs(30);

/// The dispatch error a bound produces: the command went, and no reply came
/// before the bound. Shared so a caller can tell it from a connection that is
/// down, which is not the same problem.
pub(crate) const NO_REPLY: &str = "the server did not answer the command";

/// The largest frame this side will read. A seat's whole conversation
/// snapshot crosses as ONE frame and the window it carries is frame-capped
/// (4,000 + 1,000), not byte-capped - measured snapshots reach 146.9 MB, so
/// tungstenite's 16 MiB frame default refused a real seat ("Message too
/// long: 17214131 > 16777216") and the socket died on every reconnect. A
/// bound rather than none, ~1.8x the measured maximum; capping the window
/// itself is #1637's work, not this half's.
const MAX_FRAME_BYTES: usize = 256 << 20;

/// The TCP stream under the socket, counting every byte the peer sends.
///
/// **The counter sits under TLS and the WebSocket both**, so a handshake
/// record, a frame header and the middle of a 3.5 MB payload are all the same
/// evidence: bytes are arriving. That is what liveness needs, because
/// tungstenite hands a message over only once the whole frame has arrived.
struct Counting {
    inner: tokio::net::TcpStream,
    seen: Arc<AtomicU64>,
}

impl AsyncRead for Counting {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            let got = buf.filled().len().saturating_sub(before);
            if got > 0 {
                self.seen.fetch_add(got as u64, Ordering::Relaxed);
            }
        }
        poll
    }
}

impl AsyncWrite for Counting {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Dial `url`, wrapping the transport in the byte counter before tungstenite
/// sees it.
///
/// **This half dials rather than asking tungstenite to**, because the counter
/// has to sit under the TLS layer: the public connect helpers own the stream,
/// and the one thing liveness needs is a stream of its own.
async fn dial(
    url: &str,
    config: WebSocketConfig,
    seen: Arc<AtomicU64>,
) -> Result<WebSocketStream<MaybeTlsStream<Counting>>, String> {
    let uri: Uri = url.parse().map_err(|why| format!("{url} is not an address: {why}"))?;
    let secure = uri.scheme_str() == Some("wss");
    let host = uri.host().ok_or_else(|| format!("{url} names no host"))?;
    // An IPv6 literal arrives bracketed, and `connect` wants it bare.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let tcp = tokio::net::TcpStream::connect((host, port)).await.map_err(|why| why.to_string())?;
    // **Nagle is left as the OS sets it, as the replaced dial did**: it passed
    // `disable_nagle: false`, so nothing about this socket's latency may move
    // with the dial's rewrite.
    let stream = Counting { inner: tcp, seen };
    let request = url.into_client_request().map_err(|why| why.to_string())?;
    let connector = secure.then(|| Connector::Rustls(Arc::new(tls_config())));
    let (ws, _) =
        tokio_tungstenite::client_async_tls_with_config(request, stream, Some(config), connector)
            .await
            .map_err(|why| why.to_string())?;
    Ok(ws)
}

/// The trust a `wss` dial carries: the OS roots, as tungstenite's own
/// `rustls-tls-native-roots` connect path uses.
fn tls_config() -> rustls::ClientConfig {
    let rustls_native_certs::CertificateResult { certs, errors, .. } =
        rustls_native_certs::load_native_certs();
    if !errors.is_empty() {
        tauri_plugin_log::log::debug!("native root certificate load errors: {errors:?}");
    }
    let mut roots = rustls::RootCertStore::empty();
    let (added, ignored) = roots.add_parsable_certificates(certs);
    if added == 0 {
        tauri_plugin_log::log::debug!(
            "no native root certificate parsed, so a wss dial has nothing to trust (ignored {ignored})"
        );
    }
    rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth()
}

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
    /// browser state and errors alike. The receiver reads `kind`. `bytes` is
    /// the raw frame's length and `parse_ms` how long this half's parse took -
    /// the page's perf log carries both for a snapshot (#2513).
    Message { value: Value, bytes: usize, parse_ms: f64 },
    /// A subscribe frame finished its write, with how long it waited in this
    /// half (the queue plus the write) - the page's perf log times the switch
    /// to it (#2513).
    Written { key: String, declared: bool, waited_ms: f64 },
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
            Self::Message { .. } => "message",
            Self::Written { .. } => "written",
            Self::Refused { .. } => "refused",
            Self::Malformed(_) => "malformed",
        }
    }
}

/// One subscription this side believes the server holds, in first-asked
/// order, with the declaration it was asked with and **how many open ones it
/// stands for**. The server counts subscriptions, so the count is carried
/// too: a reconnect re-asks it once per open one, and a single unsubscribe
/// decrements rather than dropping the entry a second caller still needs.
struct Held {
    key: String,
    what: Value,
    answering: bool,
    /// Whether the read this subscription was last asked with is the review
    /// page's own. Kept on the record rather than on the ask, so a reconnect
    /// re-asks the same kind of read the page wanted - a review open that
    /// dropped must not come back as an ordinary record that labels its base
    /// unverified.
    review: bool,
    count: usize,
}

impl Held {
    fn subscribe_frame(&self) -> Message {
        Message::Text(
            json!({
                "kind": "subscribe",
                "what": self.what,
                "answering": self.answering,
                "review": self.review,
            })
            .to_string()
            .into(),
        )
    }
}

/// The held list as one place: a fresh subject appends, a repeat raises its
/// flags in place and adds to its count, an unsubscribe takes one off. Both
/// the connected loop and the disconnected wait fold through here, so their
/// bookkeeping cannot drift.
fn hold(held: &mut Vec<Held>, what: Value, answering: bool, review: bool) -> Option<String> {
    let key = subject_key(&what);
    if key.is_empty() {
        return None;
    }
    match held.iter_mut().find(|h| h.key == key) {
        Some(h) => {
            h.answering |= answering;
            // **The last ask says what the held read is.** A page that lets
            // its review read go must be able to take the declaration back;
            // a role only rises, this one follows the page holding the seat.
            h.review = review;
            h.count += 1;
        }
        None => held.push(Held { key: key.clone(), what, answering, review, count: 1 }),
    }
    Some(key)
}

/// Ask again for a subject the page already holds, adding no reader: the
/// flags rise in place, and a subject this connection has not held yet (a
/// fresh socket the page re-attaches on) starts at one - the one the page's
/// own store is, not a second reader.
fn relist(held: &mut Vec<Held>, what: Value, answering: bool, review: bool) -> Option<String> {
    let key = subject_key(&what);
    if key.is_empty() {
        return None;
    }
    match held.iter_mut().find(|h| h.key == key) {
        Some(h) => {
            h.answering |= answering;
            h.review = review;
        }
        None => held.push(Held { key: key.clone(), what, answering, review, count: 1 }),
    }
    Some(key)
}

/// Change one held entry's declaration in place: no frame, no reader, and a
/// subject this connection does not hold has nothing to declare.
fn declare(held: &mut [Held], what: &Value, review: bool) {
    let key = subject_key(what);
    if let Some(entry) = held.iter_mut().find(|h| h.key == key) {
        entry.review = review;
    }
}

/// One subscription given back: the count comes off, and the entry - with
/// the key still outstanding for a refusal - goes only with the last one.
fn unhold(held: &mut Vec<Held>, awaiting: &mut Vec<String>, what: &Value) {
    let key = subject_key(what);
    let gone = match held.iter_mut().find(|h| h.key == key) {
        Some(h) => {
            h.count = h.count.saturating_sub(1);
            h.count == 0
        }
        None => true,
    };
    if gone {
        held.retain(|h| h.key != key);
        awaiting.retain(|k| k != &key);
    }
}

enum Outbound {
    Subscribe {
        what: Value,
        answering: bool,
        review: bool,
        /// When the page's command reached this half: the write's own wait is
        /// measured from here, so a subscribe queued behind a frame the loop
        /// is parsing is on the record (#2513).
        queued: tokio::time::Instant,
    },
    Unsubscribe {
        what: Value,
    },
    /// The seat the page has on screen, `None` when it shows none. A state
    /// assertion, re-sent by the page on every reconnect.
    Showing {
        seat: Option<Value>,
    },
    /// Give a subject back entirely: every subscription this connection
    /// holds for it, from the wire's own count rather than the page's
    /// readers - a relist raises that count without raising them.
    Release {
        what: Value,
    },
    /// Ask again for a subject this half already holds, **without adding a
    /// reader**: the page's half of a resume, where the record and the page's
    /// store both survived the drop and only the server's subscription went
    /// with the socket.
    Relist {
        what: Value,
        answering: bool,
        review: bool,
    },
    /// The held declaration only: nothing is sent, no reader is counted. A
    /// page that lets its review read go says so here, so the next reconnect
    /// re-asks an ordinary record.
    Declare {
        what: Value,
        review: bool,
    },
    More {
        conversation: Value,
        before: Option<String>,
        turns: u64,
    },
    /// One stretch of a seat's own stream: the epoch the numbers belong to,
    /// and the range asked for. Answered by a `seat_frames` frame or an
    /// `error` naming `seat_range`, so this half forwards and forgets.
    SeatRange {
        slot: Value,
        epoch: Option<String>,
        from: u64,
        to: u64,
    },
    CommitDiff {
        session: Value,
        sha: String,
    },
    /// One file's whole content at a revision, for the review page's
    /// between-hunk expansion: the path, and the scope's revision - absent
    /// for the seat's branch tip. The answer arrives as a plain server
    /// frame, so this half forwards and forgets.
    FileContent {
        session: Value,
        path: String,
        sha: Option<String>,
    },
    /// One document read: the seat whose tree the path resolves against,
    /// the path, and whether the hand-off flagged it reviewable. The answer
    /// arrives as a plain server frame, so this half forwards and forgets.
    DocumentRead {
        session: Value,
        path: String,
        reviewable: bool,
    },
    Dispatch {
        command: Value,
        reply: Option<u64>,
        sent: oneshot::Sender<Result<(), String>>,
        replied: Option<oneshot::Sender<Result<Value, String>>>,
    },
    /// A reply its caller has stopped waiting for: the entry goes, so a late
    /// answer is reported as unmatched rather than swallowed.
    ForgetReply {
        id: u64,
    },
    /// An attach whose answer has stopped being waited for.
    ForgetAttach {
        id: u64,
    },
    /// Attach this connection to a seat's browser tab as a viewer. The
    /// server's answer rides a reply: a raise it refused must fail the
    /// surface rather than leave it dark.
    BrowserView {
        seat: Value,
        tab: Option<u16>,
        reply: Option<u64>,
        answered: Option<oneshot::Sender<Result<Value, String>>>,
    },
    /// Stop viewing the attached tab: the takeover lowered.
    BrowserDetach,
    /// One input event from the controlling viewer.
    BrowserInput {
        seq: u64,
        event: forge_primitives::browser::BrowserInputEvent,
    },
    /// Take the controller role, or give it back.
    BrowserControl {
        take: bool,
    },
    /// Move through the page's history: the takeover bar's arrows.
    BrowserNav {
        back: bool,
    },
    /// A page's file chooser, answered with the person's files.
    BrowserFileAnswer {
        id: u64,
        files: Value,
    },
    /// The controller's surface changed size: device pixels and the ratio
    /// they draw at.
    BrowserResize {
        width: u32,
        height: u32,
        scale: f64,
    },
    /// One of the strip's asks - the profile's tab list - as the wire's own
    /// object with `reply_to` added. The answer is the reply body.
    StripAsk {
        body: Value,
        reply: Option<u64>,
        replied: Option<oneshot::Sender<Result<Value, String>>>,
    },
    Binary(Vec<u8>),
    /// Probe the link now, rather than waiting out the quiet: the page's
    /// half of a return from the background, where the socket may have died
    /// while the page slept. Queued with everything else, so it goes out in
    /// order and its deadline is stamped only once the write answers.
    Ping,
    Close,
}

/// The transport's handle: what the app half calls, and where its events land.
#[derive(Clone)]
pub struct Socket {
    tx: mpsc::UnboundedSender<Outbound>,
    next_reply: Arc<AtomicU64>,
    reply_bound: Duration,
}

impl Socket {
    /// Dial `url` and keep the connection up for as long as this handle
    /// lives. Events land on `events`; the task ends on [`Socket::close`].
    pub fn open(url: String, events: mpsc::UnboundedSender<SocketEvent>) -> Self {
        Self::open_with_reply_bound(url, events, Arc::new(|_| {}), Arc::new(|_| {}), REPLY_BOUND)
    }

    /// [`Socket::open`] with the browser-frame and audio sinks given: every
    /// relayed frame and each packet of the page's sound lands there, drawn
    /// or dropped by the viewer.
    pub fn open_with_frames(
        url: String,
        events: mpsc::UnboundedSender<SocketEvent>,
        frames: FrameSink,
        audio: AudioSink,
    ) -> Self {
        Self::open_with_reply_bound(url, events, frames, audio, REPLY_BOUND)
    }

    /// The same, with the reply bound stated: tests inject a short one rather
    /// than wait the shipping bound out.
    fn open_with_reply_bound(
        url: String,
        events: mpsc::UnboundedSender<SocketEvent>,
        frames: FrameSink,
        audio: AudioSink,
        reply_bound: Duration,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let next_reply = Arc::new(AtomicU64::new(1));
        let socket = Self { tx: tx.clone(), next_reply: Arc::clone(&next_reply), reply_bound };
        tokio::spawn(run(url, events, rx, frames, audio));
        socket
    }

    /// Watch a subject, declaring whether this client can answer its prompts.
    /// The flag only ever rises while the connection lives, and what is
    /// stated here is what a reconnect re-asks with.
    pub fn subscribe(&self, what: Value, answering: bool, review: bool) {
        let _ = self.tx.send(Outbound::Subscribe {
            what,
            answering,
            review,
            queued: tokio::time::Instant::now(),
        });
    }

    /// Give one subscription back.
    pub fn unsubscribe(&self, what: Value) {
        let _ = self.tx.send(Outbound::Unsubscribe { what });
    }

    /// Give a subject back entirely: its held entry, and every subscription
    /// this connection holds for it. **The count is the wire's**, which a
    /// relist can raise above the page's reader count.
    pub fn release(&self, what: Value) {
        let _ = self.tx.send(Outbound::Release { what });
    }

    /// Say which seat the page has on screen; `None` for none.
    pub fn showing(&self, seat: Option<Value>) {
        let _ = self.tx.send(Outbound::Showing { seat });
    }

    /// Probe the link now: the page's half of a return from the background,
    /// where the socket may have died while the page slept.
    pub fn ping(&self) {
        let _ = self.tx.send(Outbound::Ping);
    }

    /// Ask again for a subject this half already holds, without counting a
    /// reader - the page's own store and this half's record are still here,
    /// and only the wire's subscription died with the socket.
    pub fn relist(&self, what: Value, answering: bool, review: bool) {
        let _ = self.tx.send(Outbound::Relist { what, answering, review });
    }

    /// Change a held subscription's declaration **without asking for
    /// anything**: no frame, no boundary - only what a reconnect re-asks with.
    pub fn declare(&self, what: Value, review: bool) {
        let _ = self.tx.send(Outbound::Declare { what, review });
    }

    /// Ask for more turns of a conversation.
    pub fn more(&self, conversation: Value, before: Option<String>, turns: u64) {
        let _ = self.tx.send(Outbound::More { conversation, before, turns });
    }

    /// Ask a seat for the frames its stream emitted between two numbers.
    pub fn seat_range(&self, slot: Value, epoch: Option<String>, from: u64, to: u64) {
        let _ = self.tx.send(Outbound::SeatRange { slot, epoch, from, to });
    }

    /// One commit's own diff, answered by a `commit_diff` frame.
    pub fn commit_diff(&self, session: Value, sha: String) {
        let _ = self.tx.send(Outbound::CommitDiff { session, sha });
    }

    /// One file's whole content at a revision, answered by a `file_content`
    /// frame: the path, and the scope's revision - absent for the seat's
    /// branch tip.
    pub fn file_content(&self, session: Value, path: String, sha: Option<String>) {
        let _ = self.tx.send(Outbound::FileContent { session, path, sha });
    }

    /// One document read: the seat whose tree the path resolves against,
    /// the path, and whether the hand-off flagged it reviewable. Answered
    /// by a `document_read` frame or an `error` naming it - the answer
    /// carries the seat back, so a page holding two seats routes each
    /// answer to its own viewer.
    pub fn read_document(&self, session: Value, path: String, reviewable: bool) {
        let _ = self.tx.send(Outbound::DocumentRead { session, path, reviewable });
    }

    /// Send one command. With `reply` the call awaits the core's reply
    /// (`reply_to` is minted here, so correlation needs no caller
    /// bookkeeping); without it the call answers once the frame is on the
    /// wire. Err means the command was not sent, or was sent with no answer
    /// inside the bound: a caller cannot tell the two apart, and a retry may
    /// run a command that already executed.
    pub async fn dispatch(&self, command: Value, reply: bool) -> Result<Option<Value>, String> {
        let (sent, sent_rx) = oneshot::channel();
        if reply {
            let id = self.next_reply.fetch_add(1, Ordering::Relaxed);
            let bound = reply_bound_for(&command, self.reply_bound);
            let (tx, rx) = oneshot::channel();
            self.tx
                .send(Outbound::Dispatch { command, reply: Some(id), sent, replied: Some(tx) })
                .map_err(|_| "the socket is closed".to_owned())?;
            // **The write and the reply share one bound**: a wedged writer
            // parks the caller exactly as a reply that never comes does.
            let answer = async {
                sent_rx.await.map_err(|_| "the socket is closed".to_owned())??;
                rx.await.map_err(|_| "the socket dropped before answering".to_owned())?
            };
            return match bound {
                Some(bound) => match tokio::time::timeout(bound, answer).await {
                    Ok(received) => received.map(Some),
                    Err(_) => {
                        let _ = self.tx.send(Outbound::ForgetReply { id });
                        Err(NO_REPLY.to_owned())
                    }
                },
                None => answer.await.map(Some),
            };
        }
        // **Unbounded on purpose.** A bound can say no answer came; it cannot
        // say the frame did not cross, and the crossing is this path's whole
        // answer - so a wedged writer parks the page's promise rather than
        // reporting a failure it cannot name.
        self.tx
            .send(Outbound::Dispatch { command, reply: None, sent, replied: None })
            .map_err(|_| "the socket is closed".to_owned())?;
        sent_rx.await.map_err(|_| "the socket is closed".to_owned())??;
        Ok(None)
    }

    /// One dictation frame: a take's audio, best-effort.
    pub fn send_binary(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(Outbound::Binary(bytes));
    }

    /// Attach this connection to a seat's browser tab as a viewer, and wait
    /// for the server's answer: the profile and tab it actually attached, or
    /// the sentence the surface draws. **A raise that named no tab was
    /// resolved to the asking seat's own current one**, and this is the pair
    /// its Done lowers.
    pub async fn browser_view(&self, seat: Value, tab: Option<u16>) -> Result<Value, String> {
        let id = self.next_reply.fetch_add(1, Ordering::Relaxed);
        let (answered, answer) = oneshot::channel();
        self.tx
            .send(Outbound::BrowserView { seat, tab, reply: Some(id), answered: Some(answered) })
            .map_err(|_| "the socket is closed".to_owned())?;
        match tokio::time::timeout(ATTACH_BOUND, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the socket dropped before answering the attach".to_owned()),
            Err(_) => {
                let _ = self.tx.send(Outbound::ForgetAttach { id });
                Err("the server did not answer the attach".to_owned())
            }
        }
    }

    /// Stop viewing the attached profile: the takeover lowered.
    pub fn browser_detach(&self) {
        let _ = self.tx.send(Outbound::BrowserDetach);
    }

    /// One input event from the controlling viewer.
    pub fn browser_input(&self, seq: u64, event: forge_primitives::browser::BrowserInputEvent) {
        let _ = self.tx.send(Outbound::BrowserInput { seq, event });
    }

    /// Take the controller role, or give it back.
    pub fn browser_control(&self, take: bool) {
        let _ = self.tx.send(Outbound::BrowserControl { take });
    }

    /// Move through the page's history.
    pub fn browser_nav(&self, back: bool) {
        let _ = self.tx.send(Outbound::BrowserNav { back });
    }

    /// Answer a page's file chooser: the files the person picked, base64.
    pub fn browser_file_answer(&self, id: u64, files: Value) {
        let _ = self.tx.send(Outbound::BrowserFileAnswer { id, files });
    }

    /// The controller's surface changed size: device pixels and the ratio.
    pub fn browser_resize(&self, width: u32, height: u32, scale: f64) {
        let _ = self.tx.send(Outbound::BrowserResize { width, height, scale });
    }

    /// The tabs of this seat's profile, as the strip's rows: the reply body
    /// holds the [`forge_primitives::browser::BrowserTabList`] under `tabs`.
    pub async fn browser_tabs(&self, seat: Value) -> Result<Value, String> {
        self.ask(json!({ "kind": "browser_tabs", "seat": seat })).await
    }

    /// One strip ask, answered through its reply: `{ok: false, why}` is the
    /// server's refusal and comes back as `Err`.
    async fn ask(&self, body: Value) -> Result<Value, String> {
        let id = self.next_reply.fetch_add(1, Ordering::Relaxed);
        let (replied, answer) = oneshot::channel();
        self.tx
            .send(Outbound::StripAsk { body, reply: Some(id), replied: Some(replied) })
            .map_err(|_| "the socket is closed".to_owned())?;
        match tokio::time::timeout(REPLY_BOUND, answer).await {
            Ok(Ok(Ok(body))) if body.get("ok").and_then(Value::as_bool) == Some(true) => Ok(body),
            Ok(Ok(Ok(body))) => Err(body
                .get("why")
                .and_then(Value::as_str)
                .unwrap_or("the server refused the ask")
                .to_owned()),
            Ok(Ok(Err(why))) => Err(why),
            Ok(Err(_)) => Err("the socket dropped before answering the ask".to_owned()),
            Err(_) => {
                let _ = self.tx.send(Outbound::ForgetReply { id });
                Err("the server did not answer the ask".to_owned())
            }
        }
    }

    /// End the connection for good: no retry follows.
    pub fn close(&self) {
        let _ = self.tx.send(Outbound::Close);
    }
}

/// The command's variant name, as the wire tags it: the one key of the
/// command object, or the string a unit variant crosses as.
fn variant_of(command: &Value) -> Option<&str> {
    match command {
        Value::String(name) => Some(name.as_str()),
        Value::Object(map) if map.len() == 1 => map.keys().next().map(String::as_str),
        _ => None,
    }
}

/// The bound this dispatch gets: the shipping bound, except a despawn, whose
/// removal can outlast it.
fn reply_bound_for(command: &Value, bound: Duration) -> Option<Duration> {
    match variant_of(command) {
        Some("despawn_worker") => None,
        _ => Some(bound),
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
/// list (the reconnect re-asks it), a dispatch is refused, and everything
/// else has nowhere to go and is dropped - which is what `socket.ts`'s own
/// `isOpen()` guards do. Answers `true` when the task should end.
fn apply_offline(next: Outbound, held: &mut Vec<Held>, awaiting: &mut Vec<String>) -> bool {
    match next {
        Outbound::Close => true,
        Outbound::Subscribe { what, answering, review, .. } => {
            // The key is NOT queued here: nothing is outstanding until the
            // frame really goes, which is the reconnect's own re-ask. Queuing
            // it here as well would push it twice, and the extra entry would
            // let one refusal pop the key belonging to the next ask.
            hold(held, what, answering, review);
            false
        }
        // The page re-sends its showing on the reconnect, so a frame taken
        // while down is dropped rather than queued.
        Outbound::Showing { .. } => false,
        // The entry is already held; only its declaration rises, and no
        // reader is counted - the reconnect then asks exactly what was held.
        Outbound::Relist { what, answering, review } => {
            relist(held, what, answering, review);
            false
        }
        Outbound::Declare { what, review } => {
            declare(held, &what, review);
            false
        }
        Outbound::Unsubscribe { what } => {
            unhold(held, awaiting, &what);
            false
        }
        // **A release while down is a local drop**: the reconnect asks for
        // nothing the page has given back, and there is no wire to give it
        // to here. The outstanding key stays: the server answers in order,
        // and its refusal must land on the ask that earned it.
        Outbound::Release { what } => {
            let key = subject_key(&what);
            held.retain(|h| h.key != key);
            false
        }
        Outbound::Dispatch { sent, .. } => {
            let _ = sent.send(Err("the socket is not open".to_owned()));
            false
        }
        // A raise over a down socket fails by name rather than waiting out
        // its bound: no answer is coming. So does a strip ask.
        Outbound::BrowserView { answered: Some(answered), .. } => {
            let _ = answered.send(Err("the socket is not open".to_owned()));
            false
        }
        Outbound::StripAsk { replied: Some(replied), .. } => {
            let _ = replied.send(Err("the socket is not open".to_owned()));
            false
        }
        Outbound::More { .. }
        | Outbound::CommitDiff { .. }
        | Outbound::FileContent { .. }
        | Outbound::SeatRange { .. }
        | Outbound::DocumentRead { .. }
        | Outbound::ForgetReply { .. }
        | Outbound::ForgetAttach { .. }
        | Outbound::BrowserView { .. }
        | Outbound::BrowserDetach
        | Outbound::BrowserInput { .. }
        | Outbound::BrowserControl { .. }
        | Outbound::BrowserNav { .. }
        | Outbound::BrowserFileAnswer { .. }
        | Outbound::BrowserResize { .. }
        | Outbound::StripAsk { replied: None, .. }
        // Nothing to probe while there is no connection: the redial's own
        // open is what the page is waiting for.
        | Outbound::Ping
        | Outbound::Binary(_) => false,
    }
}

/// The frame sink a connection hands every relayed browser frame to: the
/// bytes are the host's own frame wire, already header-stripped of the
/// socket's kind tag.
pub type FrameSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// The audio sink a connection hands every packet of the viewed page's sound
/// to: the profile, sequence, rate and channels, then interleaved samples.
pub type AudioSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// The owning task: connect, read, retry - for the life of the handle.
async fn run(
    url: String,
    events: mpsc::UnboundedSender<SocketEvent>,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
    frames: FrameSink,
    audio: AudioSink,
) {
    let mut held: Vec<Held> = Vec::new();
    // The subscribes sent over the wire and not yet answered, oldest first -
    // what a refusal from the core is attributed to.
    let mut awaiting: Vec<String> = Vec::new();
    // **The cadence is `socket.ts`'s exactly**: the first dial goes at once,
    // a retry waits the current delay and only then doubles, and an open
    // resets it (its `scheduleRetry` schedules at the delay and doubles
    // after).
    let mut wait = RETRY_MS;
    let mut first = true;
    say(&events, SocketEvent::Status(Status::Connecting));

    loop {
        // **The disconnected wait.** Outbound messages are still taken, so a
        // subscribe made while the socket is down is held locally and the
        // reconnect re-asks it; a close ends the task.
        if !first {
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
            wait = wait.saturating_mul(2).min(MAX_RETRY_MS);
        }
        first = false;

        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        // **A dial the peer accepts and never finishes is abandoned at the
        // bound**, exactly as a refused one is: the retry loop is the answer
        // to both, and no status change is owed because the door already
        // says "connecting".
        // The bytes this connection has heard, counted under the socket by
        // `Counting`: liveness reads this, never the message stream.
        let seen = Arc::new(AtomicU64::new(0));
        let dial = tokio::time::timeout(DIAL_BOUND, dial(&url, config, Arc::clone(&seen)));
        let ws = match dial.await {
            Ok(Ok(ws)) => ws,
            Ok(Err(why)) => {
                // A dial that did not land says so, at DEBUG: the shipping
                // client's filter is Info, so this is read under a raised
                // level - which is what an unreachable server deserves while
                // the door already says "connecting". The reason is here so
                // that a failure the door cannot name (a scheme this half
                // cannot speak, a TLS refusal) is at least in the log.
                tauri_plugin_log::log::debug!("socket_dial_failed: {why}");
                continue;
            }
            Err(why) => {
                tauri_plugin_log::log::debug!("socket_dial_timed_out: {why}");
                continue;
            }
        };
        say(&events, SocketEvent::Status(Status::Open));
        wait = RETRY_MS;
        let (mut writer, mut reader) = ws.split();
        // **How many subscriptions the server holds for each subject on this
        // connection**, so an unsubscribe for a subject the new socket never
        // heard of is not sent and one the server holds twice is given back
        // twice. A set under-counts the second case: an escalated seat (the
        // review page first, then its session page) is two server
        // subscriptions, and a session end sends two unsubscribes.
        let mut live: HashMap<String, usize> = HashMap::new();
        // **The declaration is re-asked, once per OPEN subscription** - the
        // server counts, so a subject two callers hold is asked for twice -
        // with the flags it carried: this is how the role survives a drop.
        //
        // **A seat here was opened while the socket was DOWN.** The drop took
        // the seats the old connection carried out of this list, so a visited
        // seat is never re-asked by a resume; what is left is a seat the page
        // opened during the drop, whose first answer is still owed.
        for h in &held {
            for _ in 0..h.count {
                *live.entry(h.key.clone()).or_default() += 1;
                // Each frame's own wait, so the first one's send is not
                // reported again as every later frame's (#2513).
                let queued = tokio::time::Instant::now();
                if writer.send(h.subscribe_frame()).await.is_err() {
                    break;
                }
                // **One outstanding key per frame sent.** The server answers
                // per subscription, and one splice per answer is what keeps
                // this list the same length as the wire's (`socket.ts`'s
                // `askFor` and `answered` are the pair being mirrored).
                awaiting.push(h.key.clone());
                say(
                    &events,
                    SocketEvent::Written {
                        key: h.key.clone(),
                        declared: false,
                        waited_ms: queued.elapsed().as_secs_f64() * 1000.0,
                    },
                );
            }
        }
        let mut pending: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();
        // Attaches waiting on their answer, kept apart from a dispatch's:
        // their reply body is a verdict on the attach, not a command's value.
        let mut attaches: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();
        let mut closing = false;
        // **The link's own clock, counted in bytes.** `bytes_at` is the
        // counter's reading at the last beat that saw progress, `heard_at`
        // when that was, and `probe` the deadline of the question that is
        // out. A slow frame keeps advancing the counter, so a live peer
        // mid-frame is never called dead.
        let mut bytes_at = seen.load(Ordering::Relaxed);
        let mut heard_at = tokio::time::Instant::now();
        let mut probe: Option<tokio::time::Instant> = None;
        // Until a greeting says otherwise, the server is assumed to speak the
        // probe (the greeting is the first frame, long before the first beat).
        let mut speaks_liveness = true;
        // **The page's ask is stricter**: it can land before the greeting is
        // read, so it waits for one.
        let mut greeted = false;
        loop {
            // The next moment the link owes something: the probe's deadline
            // when one is out, else the quiet that earns a probe.
            let due = probe.map_or(heard_at + LIVENESS_EVERY, |sent| sent + LIVENESS_DEAD);
            tokio::select! {
                // **Reads first, always.** A pong can sit unread while this
                // half writes, and at the deadline the beat must not win that
                // race and reap a peer that already answered.
                biased;
                frame = reader.next() => match frame {
                    Some(Ok(Message::Text(text))) => {
                        let at = tokio::time::Instant::now();
                        let parsed = serde_json::from_str::<Value>(&text);
                        let parse_ms = at.elapsed().as_secs_f64() * 1000.0;
                        match parsed {
                            Ok(value) => {
                                // **Which server this is, before the first
                                // beat.** A pre-liveness server cannot answer
                                // a probe, and its refusal would land on a
                                // subscribe; the greeting is the answer.
                                if value.get("kind").and_then(Value::as_str) == Some("greeting") {
                                    greeted = true;
                                    if let Some(version) =
                                        value.get("version").and_then(Value::as_u64)
                                    {
                                        speaks_liveness = version >= LIVENESS_PROTOCOL;
                                    }
                                }
                                receive(
                                    &value,
                                    text.len(),
                                    parse_ms,
                                    &mut pending,
                                    &mut attaches,
                                    &mut awaiting,
                                    &events,
                                );
                            }
                            Err(why) => say(&events, SocketEvent::Malformed(why.to_string())),
                        }
                    }
                    // **A binary frame is the browser's or the microphone's,
                    // by its kind tag.** Kind 1 is one relayed browser frame:
                    // the sink draws it, and anything else on this stream is
                    // not this half's to read.
                    Some(Ok(Message::Binary(bytes))) => {
                        match bytes.first() {
                            Some(&1) => frames(&bytes[1..]),
                            Some(&2) => audio(&bytes[1..]),
                            // **A binary frame of another kind is said**, not
                            // dropped in silence: a short one has no kind at
                            // all, and both leave a reader guessing what went
                            // missing.
                            other => unreadable_binary(other.copied(), bytes.len()),
                        }
                    }
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
                            // A clean close, as the webview's own `ws.close()`
                            // sends: without the frame the peer reads the drop
                            // as a protocol error.
                            let _ = writer.send(Message::Close(None)).await;
                            break;
                        }
                        Outbound::Subscribe { what, answering, review, queued } => {
                            if let Some(key) = hold(&mut held, what, answering, review) {
                                if let Some(h) = held.iter().find(|h| h.key == key)
                                    && writer.send(h.subscribe_frame()).await.is_ok()
                                {
                                    *live.entry(key.clone()).or_default() += 1;
                                    say(
                                        &events,
                                        SocketEvent::Written {
                                            key: key.clone(),
                                            declared: false,
                                            waited_ms: queued.elapsed().as_secs_f64() * 1000.0,
                                        },
                                    );
                                }
                                awaiting.push(key);
                            }
                        }
                        Outbound::Declare { what, review } => {
                            // Only the held record moves: nothing is asked
                            // for, and the reconnect's own re-ask carries it.
                            declare(&mut held, &what, review);
                        }
                        Outbound::Relist { what, answering, review } => {
                            // **One frame, and no reader added**: the entry is
                            // held from the visit that opened it, or starts at
                            // one on a fresh connection, and counting again
                            // would leave an unsubscribe one short.
                            let key = relist(&mut held, what, answering, review);
                            if let Some(key) = key
                                && let Some(h) = held.iter().find(|h| h.key == key)
                            {
                                let queued = tokio::time::Instant::now();
                                if writer.send(h.subscribe_frame()).await.is_ok() {
                                    *live.entry(key.clone()).or_default() += 1;
                                    say(
                                        &events,
                                        SocketEvent::Written {
                                            key: key.clone(),
                                            declared: false,
                                            waited_ms: queued.elapsed().as_secs_f64() * 1000.0,
                                        },
                                    );
                                }
                                awaiting.push(key);
                            }
                        }
                        Outbound::Showing { seat } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "showing", "seat": seat }).to_string().into(),
                                ))
                                .await;
                        }
                        Outbound::Unsubscribe { what } => {
                            let key = subject_key(&what);
                            unhold(&mut held, &mut awaiting, &what);
                            // **Only what this connection is holding is
                            // given back, one entry per call**: a drop took
                            // the server's entries with it, so a seat
                            // released after a resume - before its click
                            // re-attached it - has nothing to stop, and a
                            // seat the server holds twice needs both.
                            match live.get_mut(&key) {
                                Some(count) if *count > 1 => {
                                    *count -= 1;
                                    let _ = writer
                                        .send(Message::Text(json!({ "kind": "unsubscribe", "what": what }).to_string().into()))
                                        .await;
                                }
                                Some(_) => {
                                    live.remove(&key);
                                    let _ = writer
                                        .send(Message::Text(json!({ "kind": "unsubscribe", "what": what }).to_string().into()))
                                        .await;
                                }
                                None => {}
                            }
                        }
                        Outbound::Release { what } => {
                            // **Every entry this connection holds, from the
                            // wire's count**: a relist raises it above the
                            // page's readers, and looping those would leave a
                            // subscription and its work-watch hold standing.
                            // The awaiting key stays: the server answers in
                            // order, so its refusal is this ask's, not the
                            // next store's.
                            let key = subject_key(&what);
                            held.retain(|h| h.key != key);
                            for _ in 0..live.remove(&key).unwrap_or(0) {
                                let _ = writer
                                    .send(Message::Text(json!({ "kind": "unsubscribe", "what": what.clone() }).to_string().into()))
                                    .await;
                            }
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
                        Outbound::SeatRange { slot, epoch, from, to } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "seat_range", "slot": slot, "epoch": epoch, "from": from, "to": to })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                        Outbound::CommitDiff { session, sha } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "commit_diff", "session": session, "sha": sha })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                        Outbound::FileContent { session, path, sha } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "file_content", "session": session, "path": path, "sha": sha })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                        Outbound::DocumentRead { session, path, reviewable } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "document_read", "session": session, "path": path, "reviewable": reviewable })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
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
                        Outbound::ForgetReply { id } => {
                            pending.remove(&id);
                        }
                        Outbound::ForgetAttach { id } => {
                            attaches.remove(&id);
                        }
                        Outbound::BrowserView { seat, tab, reply, answered } => {
                            let ok = writer
                                .send(Message::Text(
                                    json!({
                                        "kind": "browser_view",
                                        "seat": seat,
                                        "tab": tab,
                                        "reply_to": reply,
                                    })
                                        .to_string()
                                        .into(),
                                ))
                                .await
                                .is_ok();
                            match (ok, reply, answered) {
                                (true, Some(id), Some(tx)) => {
                                    attaches.insert(id, tx);
                                }
                                (_, _, Some(tx)) => {
                                    let _ = tx.send(Err(
                                        "the socket is open but the frame did not go".to_owned()
                                    ));
                                }
                                (_, _, None) => {}
                            }
                        }
                        Outbound::BrowserDetach => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "browser_detach" }).to_string().into(),
                                ))
                                .await;
                        }
                        Outbound::BrowserInput { seq, event } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "browser_input", "seq": seq, "event": event })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                        Outbound::BrowserControl { take } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "browser_control", "take": take })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                        Outbound::BrowserNav { back } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "browser_nav", "back": back })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                        Outbound::BrowserFileAnswer { id, files } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({ "kind": "browser_file_answer", "id": id, "files": files })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                        Outbound::BrowserResize { width, height, scale } => {
                            let _ = writer
                                .send(Message::Text(
                                    json!({
                                        "kind": "browser_resize",
                                        "width": width,
                                        "height": height,
                                        "scale": scale,
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await;
                        }
                        Outbound::StripAsk { mut body, reply, replied } => {
                            if let Some(map) = body.as_object_mut() {
                                map.insert("reply_to".to_owned(), json!(reply));
                            }
                            let ok =
                                writer.send(Message::Text(body.to_string().into())).await.is_ok();
                            if ok
                                && let (Some(id), Some(tx)) = (reply, replied)
                            {
                                pending.insert(id, tx);
                            }
                        }
                        Outbound::Binary(bytes) => {
                            // Best-effort by contract - the take's own ring
                            // holds what did not go - but not silent: a frame
                            // that could not be sent is a fact about the
                            // connection (debug: it repeats with the take's
                            // own flush cadence).
                            if let Err(why) = writer.send(Message::Binary(bytes.into())).await {
                                tauri_plugin_log::log::debug!(
                                    "socket_frame_not_sent: {why}"
                                );
                            }
                        }
                        // **A probe the page asked for**, on a return from
                        // the background: the socket may have died while the
                        // page slept and its own timer with it.
                        Outbound::Ping => {
                            // **The page's ask needs a greeting that says the
                            // server speaks it.** The timed beat is safe
                            // without one - it is 15 s out - but a return can
                            // land before the greeting is read, and a
                            // pre-liveness server's answer would be booked on
                            // a subscribe.
                            if !greeted || !speaks_liveness {
                                continue;
                            }
                            // **A probe already out stands, exactly as the
                            // page's own deadline does.** A second ask must
                            // neither push the deadline out nor re-snapshot
                            // the counter: bytes that answer the first probe
                            // would stop counting as progress, and the beat
                            // would reap a live link at the first deadline.
                            if probe.is_some() {
                                continue;
                            }
                            if writer.send(ping_frame()).await.is_err() {
                                break;
                            }
                            probe = Some(tokio::time::Instant::now());
                            // **Bytes already here are not the answer.** The
                            // counter's reading is snapshotted with the ask,
                            // so only progress AFTER it can clear the probe;
                            // otherwise traffic that predates the question
                            // would satisfy it at the next beat.
                            bytes_at = seen.load(Ordering::Relaxed);
                        }
                    }
                }
                // **The liveness beat, judged in bytes.** A frame still
                // arriving advances the counter; that is progress, not
                // silence, whatever tungstenite has handed over yet.
                () = tokio::time::sleep_until(due) => {
                    let now_seen = seen.load(Ordering::Relaxed);
                    if now_seen != bytes_at {
                        bytes_at = now_seen;
                        heard_at = tokio::time::Instant::now();
                        probe = None;
                        // Progress is the answer; the next moment owed is the
                        // quiet's, not this deadline's.
                        continue;
                    }
                    match probe {
                        Some(sent) => {
                            tauri_plugin_log::log::debug!(
                                "socket_liveness_dead: no byte answered a probe in {}ms",
                                sent.elapsed().as_millis(),
                            );
                            break;
                        }
                        None => {
                            // **A pre-liveness server is not asked.** Its
                            // refusal would be booked on a subscribe; the
                            // quiet re-arms from now so the beat cannot spin.
                            if !speaks_liveness {
                                heard_at = tokio::time::Instant::now();
                                continue;
                            }
                            // **The deadline is stamped only once the send
                            // has answered**: a probe queued behind a busy
                            // write owes nothing until it reaches the peer.
                            //
                            // **The send itself is unbounded**, like every
                            // write on this half: a peer that stopped reading
                            // can hold it open, and the beat is then as stuck
                            // as the link. Binding it is the server's
                            // `write_within` shape and its own change.
                            if writer.send(ping_frame()).await.is_err() {
                                break;
                            }
                            probe = Some(tokio::time::Instant::now());
                        }
                    }
                }
            }
        }
        // **The connection left: whatever it was holding answers now**, the
        // role goes back, and the asks a drop killed are forgotten so a later
        // refusal finds its own. A close this side asked for says so, the way
        // `socket.ts` tells its own close from a drop.
        let why = if closing {
            "the connection was closed"
        } else {
            "the socket dropped before answering"
        };
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(why.to_owned()));
        }
        for (_, tx) in attaches.drain() {
            let _ = tx.send(Err(why.to_owned()));
        }
        awaiting.clear();
        // **A seat is the page's own to re-open.** The seats this connection
        // carried leave the list with it, exactly as `socket.ts` clears them
        // on close: a visited seat is not re-asked by a resume, and only the
        // shown one re-attaches (the page's `relist`). What a subscribe or
        // relist ADDS while the socket is down stays, because its first ask
        // never went.
        if !closing {
            held.retain(|h| h.what.is_string());
        }
        if closing {
            say(&events, SocketEvent::Status(Status::Closed));
            return;
        }
        say(&events, SocketEvent::Status(Status::Connecting));
    }
}

/// The wire's liveness probe, built in one place because two paths send it.
fn ping_frame() -> Message {
    Message::Text(json!({ "kind": "ping" }).to_string().into())
}

/// Say a binary frame this half cannot read.
///
/// **Info, not debug**: this module is not one of the default directives'
/// named targets, so a debug line here would never reach `forge.log` - the
/// refusal would be silent in the one place a reader looks.
fn unreadable_binary(kind: Option<u8>, bytes: usize) {
    tauri_plugin_log::log::info!(
        "a binary frame carried an unknown kind (event_name client_socket_binary_unknown, \
         kind {kind:?}, {bytes} bytes)"
    );
}

fn say(events: &mpsc::UnboundedSender<SocketEvent>, event: SocketEvent) {
    let _ = events.send(event);
}

/// One server frame: replies are correlated here, a refusal goes to the
/// oldest subscribe still waiting, the role frame becomes an event, and
/// everything else crosses verbatim.
fn receive(
    value: &Value,
    bytes: usize,
    parse_ms: f64,
    pending: &mut HashMap<u64, oneshot::Sender<Result<Value, String>>>,
    attaches: &mut HashMap<u64, oneshot::Sender<Result<Value, String>>>,
    awaiting: &mut Vec<String>,
    events: &mpsc::UnboundedSender<SocketEvent>,
) {
    match value.get("kind").and_then(Value::as_str) {
        Some("reply") => {
            let id = value.get("reply_to").and_then(Value::as_u64);
            let body = value.get("body").cloned().unwrap_or(Value::Null);
            // **An attach's answer first**: its body is a verdict, and a
            // refusal is the surface's own sentence.
            if let Some(tx) = id.and_then(|id| attaches.remove(&id)) {
                let verdict = if body.get("ok").and_then(Value::as_bool) == Some(true) {
                    match (
                        body.get("profile").and_then(Value::as_str),
                        body.get("tab").and_then(Value::as_u64),
                    ) {
                        (Some(profile), Some(tab)) => Ok(json!({ "profile": profile, "tab": tab })),
                        _ => {
                            Err("the server attached without naming the profile and tab".to_owned())
                        }
                    }
                } else {
                    Err(body
                        .get("why")
                        .and_then(Value::as_str)
                        .unwrap_or("the browser could not be attached")
                        .to_owned())
                };
                let _ = tx.send(verdict);
                return;
            }
            match id.and_then(|id| pending.remove(&id)) {
                Some(tx) => {
                    let _ = tx.send(Ok(body));
                }
                // A reply nothing is waiting on is a fact about the two
                // sides' reply spaces, and a silent drop is a command whose
                // failure nobody can read (`socket.ts` reports the same).
                // WARN: a dispatch that gave up on its bound leaves its late
                // reply to land here, and the Info filter would swallow a
                // debug line in the shipping client.
                None => tauri_plugin_log::log::warn!(
                    "socket_reply_unmatched: id {id:?} has nothing waiting on it"
                ),
            }
        }
        Some("snapshot") => {
            // **A snapshot answers ONE subscription**, so one of its keys
            // comes off the outstanding list: without this a refusal pops
            // the OLDEST key ever queued - one already answered - and lands
            // on the wrong store while the refused one reads as loading
            // forever. One, not all: a subject two callers hold has two asks
            // outstanding and the second answer is still owed.
            if let Some(subject) = value.get("subject") {
                let key = subject_key(subject);
                if let Some(at) = awaiting.iter().position(|held| *held == key) {
                    awaiting.remove(at);
                }
            }
            say(events, SocketEvent::Message { value: value.clone(), bytes, parse_ms });
        }
        Some("error") => {
            // **An unknown subject comes back as `client_message`**, because
            // the server could not read the frame that named it - a v8 server
            // and a `board` subscribe, above all. The refused subscription is
            // the one this side is still waiting on, so the refusal is
            // attributed to it; left as a plain frame the page would wait on
            // an answer that is never coming (#2517).
            let what = value.get("what").and_then(Value::as_str);
            if matches!(what, Some("subscribe" | "client_message")) && !awaiting.is_empty() {
                let key = awaiting.remove(0);
                let why = value.get("why").and_then(Value::as_str).unwrap_or_default().to_owned();
                say(events, SocketEvent::Refused { key, why });
                return;
            }
            say(events, SocketEvent::Message { value: value.clone(), bytes, parse_ms });
        }
        _ => say(events, SocketEvent::Message { value: value.clone(), bytes, parse_ms }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

    /// How long a paused-clock test waits on real I/O before naming the
    /// property that did not happen.
    ///
    /// **The clock is virtual but the socket is not.** A loaded gate was
    /// measured holding the watcher family for 7-8 s, so this sits well
    /// above that rather than near the value a quiet machine gives.
    const PAUSED_WAIT: Duration = Duration::from_secs(15);

    /// **An unknown binary frame is said at a level the client keeps.** This
    /// module is not a target the default directives name, so the line at
    /// `debug` never reached `forge.log`; the level is the mutation this test
    /// reds.
    #[test]
    fn an_unknown_binary_frame_is_said_at_info() {
        let _held = crate::log_capture::capture();
        unreadable_binary(Some(7), 12);
        let lines = crate::log_capture::lines();
        assert!(
            lines.iter().any(|line| {
                line.contains("client_socket_binary_unknown")
                    && line.contains("Some(7)")
                    && line.contains("12 bytes")
            }),
            "the refusal names the kind and the size: {lines:?}",
        );
    }

    /// **The desktop's only subscribe mark is this event (#2513).** A
    /// subscribe crosses as a command; the frame's own write is what says it
    /// left, and the page's perf log reads that as the subscribe mark.
    #[tokio::test]
    async fn a_subscribe_write_is_reported_to_the_page() {
        let stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        socket.subscribe(json!("home"), true, false);
        let SocketEvent::Written { key, declared, waited_ms } = event_of(&mut rx, "written").await
        else {
            unreachable!("filtered to written");
        };
        assert_eq!(key, "home", "the mark names the subject the frame asked for");
        assert!(!declared, "a held subscription is not the role's declare");
        assert!(waited_ms.is_finite(), "the wait is measured, not a placeholder");
    }

    /// A stub forge: accepts connections, forwards every text frame it hears,
    /// and broadcasts whatever the test pushes. `kill` drops the live
    /// connections while the listener lives on, which is what a reconnect
    /// lands on.
    pub(crate) struct Stub {
        pub(crate) url: String,
        pub(crate) heard: mpsc::UnboundedReceiver<Value>,
        pub(crate) say: tokio::sync::broadcast::Sender<Value>,
        pub(crate) kill: tokio::sync::broadcast::Sender<()>,
        /// Every text frame a connection sent, broadcast so a helper can
        /// answer the client's probes while the test keeps its own `heard`
        /// stream for what it is asserting.
        pub(crate) spoken: tokio::sync::broadcast::Sender<Value>,
        /// Kept so the broadcast always has a receiver: `send` refuses when
        /// nobody is subscribed, and a test may push before the socket's own
        /// per-connection task has joined.
        _keep: tokio::sync::broadcast::Receiver<Value>,
        /// The same, for the kill channel: a broadcast with no receiver
        /// refuses to send.
        _kill_keep: tokio::sync::broadcast::Receiver<()>,
        /// And for the spoken channel, for the same reason.
        _spoken_keep: tokio::sync::broadcast::Receiver<Value>,
    }

    pub(crate) async fn stub() -> Stub {
        stub_mode(false).await
    }

    /// A stub whose drop is a reset rather than a goodbye: the peer is gone,
    /// so the client's next write fails instead of landing in a half-closed
    /// socket's buffer.
    pub(crate) async fn resetting_stub() -> Stub {
        stub_mode(true).await
    }

    async fn stub_mode(reset: bool) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let addr = listener.local_addr().expect("the bound address");
        let (heard_tx, heard) = mpsc::unbounded_channel();
        let (say, keep) = tokio::sync::broadcast::channel::<Value>(64);
        let (kill, kill_keep) = tokio::sync::broadcast::channel(8);
        let (spoken, spoken_keep) = tokio::sync::broadcast::channel::<Value>(64);
        let say_for_task = say.clone();
        let kill_for_task = kill.clone();
        let spoken_for_task = spoken.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                if reset {
                    // A zero linger turns the drop into a reset, which is the
                    // state a client's next write fails on.
                    let held = socket2::SockRef::from(&stream);
                    let _ = held.set_linger(Some(Duration::ZERO));
                }
                let heard_tx = heard_tx.clone();
                let spoken = spoken_for_task.clone();
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
                                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                                        let _ = spoken.send(value.clone());
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
            spoken,
            _keep: keep,
            _kill_keep: kill_keep,
            _spoken_keep: spoken_keep,
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

        /// [`Stub::heard_until`] for a paused clock.
        ///
        /// **A virtual timeout expires before real I/O gets a turn** when the
        /// runtime has nothing else to run, so a paused test waits on a
        /// real-time guard and lets the clock creep a millisecond per poll.
        pub(crate) async fn heard_until_paused(
            &mut self,
            mut ok: impl FnMut(&Value) -> bool,
        ) -> Value {
            let real = std::time::Instant::now() + PAUSED_WAIT;
            loop {
                while let Ok(value) = self.heard.try_recv() {
                    if ok(&value) {
                        return value;
                    }
                }
                assert!(std::time::Instant::now() < real, "a frame arrived");
                tokio::time::sleep(Duration::from_millis(1)).await;
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

    /// [`next_event`] for a paused clock, for the reason
    /// [`Stub::heard_until_paused`] states: a virtual timeout would expire
    /// before the real socket had a turn.
    ///
    /// `what` is the property the wait is for, so a wait that never ends
    /// names what did not happen rather than reporting a bare timeout.
    async fn next_event_paused(
        rx: &mut mpsc::UnboundedReceiver<SocketEvent>,
        what: &str,
    ) -> SocketEvent {
        let real = std::time::Instant::now() + PAUSED_WAIT;
        loop {
            if let Ok(event) = rx.try_recv() {
                return event;
            }
            assert!(std::time::Instant::now() < real, "{what}");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Let the runtime run real I/O for a moment under a paused clock.
    ///
    /// **A virtual `advance` wakes a task; it does not promise the task has
    /// run before the call returns.** An assertion that must not outrun the
    /// task waits on real time here, which the blocking pool spends while the
    /// runtime keeps polling the task the advance woke.
    async fn settle(millis: u64) {
        tokio::task::spawn_blocking(move || std::thread::sleep(Duration::from_millis(millis)))
            .await
            .expect("the wait runs");
    }

    /// [`connected`] for a paused clock: see [`next_event_paused`].
    async fn connected_paused(stub: &Stub) -> (Socket, mpsc::UnboundedReceiver<SocketEvent>) {
        let (socket, mut rx) = opened(stub);
        loop {
            if let SocketEvent::Status(Status::Open) =
                next_event_paused(&mut rx, "the socket opened").await
            {
                break;
            }
        }
        (socket, rx)
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
            SocketEvent::Message { value, bytes, .. } => {
                assert_eq!(value, greeting);
                assert!(bytes > 0, "a message carries its raw frame's length (#2513)");
            }
            other => panic!("a message expected, got {other:?}"),
        }
        let _ = socket;
    }

    /// The pessimistic rate a debug build's frame crosses at on a loaded
    /// machine: a 70 MiB frame measured ~19 s at load 41-64, which is about
    /// 3.7 MiB/s, so 3 MiB/s is the floor a bound is derived from (#2273).
    const CROSS_MIB_PER_SEC: u64 = 3;

    /// How long a frame of `bytes` may take to cross, derived from its own
    /// weight rather than a number chosen for a quiet machine.
    fn cross_bound(bytes: usize) -> Duration {
        let mib = u64::try_from(bytes / (1024 * 1024)).unwrap_or(u64::MAX);
        Duration::from_secs(2) + Duration::from_secs(mib.div_ceil(CROSS_MIB_PER_SEC))
    }

    /// **A frame past both of tungstenite's defaults is delivered, not
    /// refused.** A seat's whole conversation snapshot crosses as ONE frame,
    /// and the kdp-test seat's is 16.4 MiB - past the 16 MiB frame default,
    /// which killed the socket on every reconnect (the app's "Reading the
    /// conversation..." / "Reconnecting" loop). Sized past the 64 MiB message
    /// default too, so both caps the connect sets are load-bearing here.
    #[tokio::test]
    async fn carries_a_frame_past_the_old_default_caps() {
        let stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        let blob = "x".repeat(70 * 1024 * 1024);
        let bytes = blob.len();
        let snapshot = json!({ "kind": "snapshot", "subject": "home", "data": { "blob": blob } });
        stub.say.send(snapshot.clone()).expect("the stub broadcasts");
        // A frame this size takes seconds to serialize, cross and parse, so it
        // gets its own bound, derived from its size - the shared two seconds
        // fit the small ones.
        let event = tokio::time::timeout(cross_bound(bytes), rx.recv())
            .await
            .expect("the oversized frame within the bound")
            .expect("the socket still feeding");
        match event {
            SocketEvent::Message { value, .. } => {
                assert_eq!(value, snapshot, "the whole frame arrives intact");
            }
            // The production failure, named: the read that refuses the
            // frame is what killed the socket.
            SocketEvent::Malformed(why) => panic!("the frame was refused: {why}"),
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
        socket.subscribe(json!("home"), true, false);
        let first = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(first["what"], json!("home"));
    }

    /// A document read crosses as its own frame, with the seat, the path
    /// and the reviewable flag the page asked with - the answer arrives as
    /// a plain server frame, so this half forwards and forgets.
    #[tokio::test]
    async fn forwards_a_document_read_with_its_seat_and_flags() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        let seat = json!({ "org": "Busytools", "project": "forge", "label": "lead" });

        socket.read_document(seat.clone(), "docs/readme.md".to_owned(), true);

        let heard = stub.heard_until(|v| v["kind"] == "document_read").await;
        assert_eq!(heard["session"], seat, "the seat whose tree the path resolves against");
        assert_eq!(heard["path"], "docs/readme.md");
        assert_eq!(heard["reviewable"], true);
    }

    /// The declaration rides every subscribe, and a refusal goes to the
    /// OLDEST subscribe still waiting - `socket.ts`'s `awaiting` queue.
    #[tokio::test]
    async fn declares_the_flags_it_was_given_and_attributes_a_refusal_to_the_oldest() {
        let mut stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        socket.subscribe(json!("home"), true, false);
        socket.subscribe(json!("usage"), true, false);
        let first = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(first["what"], json!("home"));
        assert_eq!(first["answering"], true);
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

    /// **An attach's refusal comes back as its own sentence**, so the surface
    /// can fail on it rather than going live over nothing.
    #[tokio::test]
    async fn an_attach_answers_its_refusal_by_name() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        let waiting = tokio::spawn({
            let socket = socket.clone();
            async move {
                socket
                    .browser_view(json!({ "org": "a", "project": "b", "label": "lead" }), None)
                    .await
            }
        });
        let sent = stub.heard_until(|v| v["kind"] == "browser_view").await;
        assert_eq!(sent["tab"], Value::Null, "no tab is the seat's current one");
        let id = sent["reply_to"].as_u64().expect("the asker minted a reply id");
        stub.say
            .send(json!({
                "kind": "reply",
                "reply_to": id,
                "body": { "ok": false, "why": "the profile 'held' belongs to another seat" },
            }))
            .expect("broadcast");
        let why = waiting.await.expect("the task joins").expect_err("the attach was refused");
        assert!(why.contains("belongs to another seat"), "{why}");
    }

    /// **A lowered takeover gives its viewer up on the wire.** Both shells
    /// send this - the phone's picture is a canvas in its page, but it
    /// attaches as a viewer like the desktop's overlay, and a lower that
    /// said nothing kept its profile warm.
    #[tokio::test]
    async fn lowering_sends_the_detach() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        socket.browser_detach();
        let sent = stub.heard_until(|v| v["kind"] == "browser_detach").await;
        assert_eq!(sent["kind"], "browser_detach", "the lowered viewer says so");
    }

    /// **An attach's answer names the profile and tab it landed on**, so the
    /// page can filter frames by both rather than by a guess.
    #[tokio::test]
    async fn an_attach_answers_the_tab_it_landed_on() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        let waiting = tokio::spawn({
            let socket = socket.clone();
            async move {
                socket
                    .browser_view(json!({ "org": "a", "project": "b", "label": "lead" }), Some(2))
                    .await
            }
        });
        let sent = stub.heard_until(|v| v["kind"] == "browser_view").await;
        assert_eq!(sent["tab"], json!(2), "the chosen tab crosses with the ask");
        let id = sent["reply_to"].as_u64().expect("the asker minted a reply id");
        stub.say
            .send(json!({
                "kind": "reply",
                "reply_to": id,
                "body": { "ok": true, "profile": "Busytools/work", "tab": 2 },
            }))
            .expect("broadcast");
        let landed = waiting.await.expect("the task joins").expect("the attach landed");
        assert_eq!(landed["profile"], json!("Busytools/work"));
        assert_eq!(landed["tab"], json!(2));
    }

    /// **A strip ask carries its fields and answers through its own reply**:
    /// the tabs on success, the server's sentence on a refusal.
    #[tokio::test]
    async fn a_tab_list_answers_through_its_reply() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        let asking = tokio::spawn({
            let socket = socket.clone();
            async move {
                socket.browser_tabs(json!({ "org": "a", "project": "b", "label": "lead" })).await
            }
        });
        let sent = stub.heard_until(|v| v["kind"] == "browser_tabs").await;
        assert_eq!(sent["seat"]["label"], "lead", "the seat crosses with the ask");
        let id = sent["reply_to"].as_u64().expect("the asker minted a reply id");
        stub.say
            .send(json!({
                "kind": "reply",
                "reply_to": id,
                "body": { "ok": true, "tabs": { "profile": "Busytools/work", "rows": [] } },
            }))
            .expect("broadcast");
        let body = asking.await.expect("the task joins").expect("the tabs");
        assert_eq!(body["tabs"]["profile"], json!("Busytools/work"));

        let refused = tokio::spawn({
            let socket = socket.clone();
            async move {
                socket.browser_tabs(json!({ "org": "a", "project": "b", "label": "lead" })).await
            }
        });
        let sent = stub.heard_until(|v| v["kind"] == "browser_tabs").await;
        let id = sent["reply_to"].as_u64().expect("a reply id");
        stub.say
            .send(json!({
                "kind": "reply",
                "reply_to": id,
                "body": { "ok": false, "why": "that seat has no browser profile" },
            }))
            .expect("broadcast");
        let why = refused.await.expect("the task joins").expect_err("the read was refused");
        assert!(why.contains("no browser profile"), "{why}");
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

    /// **A dispatch taken while the socket is away is refused, not held.**
    /// The reconnect re-asks what was subscribed; a command is not something
    /// to replay, so the page is told it did not go (#2688).
    #[tokio::test]
    async fn refuses_a_dispatch_taken_while_the_socket_is_away() {
        let (sent, sent_rx) = oneshot::channel();
        let mut held = Vec::new();
        let mut awaiting = Vec::new();
        assert!(
            !apply_offline(
                Outbound::Dispatch { command: json!({}), reply: None, sent, replied: None },
                &mut held,
                &mut awaiting,
            ),
            "an offline dispatch does not end the task",
        );
        let why = sent_rx.await.expect("the page's channel is answered").expect_err("refused");
        assert_eq!(why, "the socket is not open", "the page is told why nothing went");
    }

    /// **A frame the writer could not put on the wire is not a success.**
    ///
    /// This is the shell's half of #2688: the page's no-reply dispatch
    /// answers `Ok` only after the writer has the frame, so a write that
    /// fails must answer `Err` rather than let a caller clear state for a
    /// command nobody received. The peer resets here, and the dispatch is
    /// taken after it has gone, which is the window the page cannot see.
    #[tokio::test]
    async fn refuses_a_frame_the_writer_could_not_put_on_the_wire() {
        let stub = resetting_stub().await;
        let (socket, _rx) = connected(&stub).await;
        stub.kill.send(()).expect("the peer resets");

        let why = tokio::time::timeout(
            Duration::from_secs(2),
            socket.dispatch(json!({ "cancel": {} }), false),
        )
        .await
        .expect("the refusal lands fast")
        .expect_err("a command the writer could not send is refused");
        assert!(why.contains("did not go"), "the write's own failure is the answer: {why}");
    }

    /// A reconnect re-asks every held subscription once, with the flags it
    /// declared - the role's whole survival depends on this.
    #[tokio::test]
    async fn re_asks_every_held_subscription_after_a_drop() {
        let mut stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        socket.subscribe(json!("home"), true, false);
        let first = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(first["what"], json!("home"));
        // Drop the connection under the socket; the listener lives on, so the
        // reconnect lands and its re-ask is what this reads.
        stub.kill.send(()).expect("the stub drops its connections");
        loop {
            if let SocketEvent::Status(Status::Connecting) = next_event(&mut rx).await {
                break;
            }
        }
        let again = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(again["what"], json!("home"));
        assert_eq!(again["answering"], true, "the declaration is carried, not defaulted");
    }

    /// **A seat's own asks carry the review scope.** A seat is never re-asked
    /// by a resume (the page re-attaches it, so the flag would mean nothing
    /// there), which leaves the two frames a page really sends for a seat: its
    /// open and a relist. A frame that dropped the scope would turn a review
    /// page's read into an ordinary record.
    #[tokio::test]
    async fn a_seat_frame_carries_the_review_scope() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        let seat = json!({ "session": { "org": "TestOrg", "project": "proj", "label": "lead" } });

        socket.subscribe(seat.clone(), false, true);
        let opened = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(opened["what"], seat, "the seat's open went");
        assert_eq!(opened["review"], json!(true), "carrying the page's own read");

        socket.relist(seat, false, false);
        let relisted = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(relisted["review"], json!(false), "and a relist's own scope wins");
    }

    /// **A seat is the page's own to re-open after a drop.** The subscription
    /// the page left behind is not re-asked by this half; the shown seat is
    /// re-attached by the page's own `relist`, and the rest wait for a click.
    #[tokio::test]
    async fn a_reconnect_re_asks_a_subject_but_never_a_seat() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        socket.subscribe(json!("home"), true, false);
        socket.subscribe(
            json!({ "session": { "org": "TestOrg", "project": "proj", "label": "lead" } }),
            true,
            false,
        );
        let _ = stub.heard_until(|v| v["kind"] == "subscribe" && v["what"] == json!("home")).await;
        let _ = stub
            .heard_until(|v| v["kind"] == "subscribe" && v["what"]["session"]["label"] == "lead")
            .await;

        stub.kill.send(()).expect("the stub drops its connections");
        let again = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(again["what"], json!("home"), "the reconnect stopped re-asking a subject");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stub.heard.recv()).await.is_err(),
            "a seat re-attached itself on resume instead of waiting for a click",
        );
    }

    /// **A relist asks once and adds no reader**, which is what keeps the
    /// unsubscribe count level with the server's after a resume.
    #[tokio::test]
    async fn a_relist_asks_once_without_raising_the_count() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        socket.subscribe(json!("home"), false, false);
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;

        socket.relist(json!("home"), true, false);
        let relisted = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(relisted["what"], json!("home"));
        assert_eq!(relisted["answering"], true, "the relist's declaration rode the frame");

        // One held reader, so one re-ask: a relist that counted would ask twice.
        stub.kill.send(()).expect("the stub drops its connections");
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stub.heard.recv()).await.is_err(),
            "the relist raised the count, so the reconnect asked twice",
        );
    }

    /// **A seat opened while the socket was down is asked when it comes
    /// back.** Its first subscribe never went, so this is not the resume rule
    /// - only a seat that was live on the old connection is left to the page.
    #[tokio::test]
    async fn a_seat_opened_while_down_is_asked_on_reconnect() {
        let mut stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        stub.kill.send(()).expect("the stub drops its connections");
        loop {
            if let SocketEvent::Status(Status::Connecting) = next_event(&mut rx).await {
                break;
            }
        }
        let seat = json!({ "session": { "org": "TestOrg", "project": "proj", "label": "w1" } });
        socket.subscribe(seat.clone(), true, false);

        let asked = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(asked["what"], seat, "a seat opened during the drop was never asked for");
        assert_eq!(asked["answering"], true, "with the declaration the page made");
    }

    /// **A relist on a fresh connection asks once**, starting the entry at the
    /// one reader the page's own store is: a page-driven reconnect builds a
    /// new socket, and the shown seat re-attaches into a list that is empty.
    #[tokio::test]
    async fn a_relist_on_a_fresh_connection_asks_once() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        let seat = json!({ "session": { "org": "TestOrg", "project": "proj", "label": "lead" } });

        socket.relist(seat.clone(), true, false);
        let asked = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(asked["what"], seat, "the relist asked for the seat");
        assert_eq!(asked["answering"], true, "with the declaration the page holds");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stub.heard.recv()).await.is_err(),
            "the relist asked more than once",
        );
    }

    /// **A showing crosses as its own frame**, `null` included: it is the
    /// wire's state assertion for the seat on screen, and "nothing of mine"
    /// is one of its states.
    #[tokio::test]
    async fn a_showing_crosses_as_its_own_frame() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        let seat = json!({ "org": "TestOrg", "project": "proj", "label": "lead" });

        socket.showing(Some(seat.clone()));
        let shown = stub.heard_until(|v| v["kind"] == "showing").await;
        assert_eq!(shown["seat"], seat, "the seat rode the frame");

        socket.showing(None);
        let cleared = stub.heard_until(|v| v["kind"] == "showing").await;
        assert_eq!(cleared["seat"], Value::Null, "nothing on screen crosses as null");
    }

    /// **A seat released after a resume has nothing on the wire to give back**
    /// until its click re-attached it: the drop took the server's entry, and
    /// the reconnect never asked for it again.
    #[tokio::test]
    async fn an_unsubscribe_for_a_seat_the_new_connection_never_asked_is_not_sent() {
        let mut stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        let seat = json!({ "session": { "org": "TestOrg", "project": "proj", "label": "w1" } });
        socket.subscribe(seat.clone(), true, false);
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;

        stub.kill.send(()).expect("the stub drops its connections");
        loop {
            if let SocketEvent::Status(Status::Open) = next_event(&mut rx).await {
                break;
            }
        }
        socket.unsubscribe(seat);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stub.heard.recv()).await.is_err(),
            "an unsubscribe went for a seat the new connection never subscribed",
        );
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

    /// **A snapshot answers its subscription**, so a refusal that follows
    /// pops the NEXT key and not one already answered (`socket.test.ts`
    /// pins the same three cases).
    #[tokio::test]
    async fn a_snapshot_takes_its_key_off_the_outstanding_list() {
        let mut stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        socket.subscribe(json!("home"), true, false);
        let _ = stub.heard_until(|v| v["kind"] == "subscribe" && v["what"] == json!("home")).await;
        stub.say
            .send(json!({ "kind": "snapshot", "subject": "home", "data": {} }))
            .expect("the stub answers");
        // The answer really landed before the second ask goes out.
        let _ = event_of(&mut rx, "message").await;
        socket.subscribe(
            json!({ "session": { "org": "a", "project": "b", "label": "lead" } }),
            true,
            false,
        );
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;
        stub.say
            .send(json!({ "kind": "error", "what": "subscribe", "why": "no session there" }))
            .expect("the stub refuses");
        let refused = event_of(&mut rx, "refused").await;
        let SocketEvent::Refused { key, .. } = refused else { unreachable!("filtered") };
        assert_eq!(
            key, "session:a\u{0}b\u{0}lead",
            "the refusal lands on the ask that is really outstanding, not the answered one"
        );
    }

    /// **A subscription the server holds twice is given back twice.** A role
    /// escalation is the ordinary way a subject gets two entries, and a set
    /// would send the first unsubscribe and swallow the second, leaving a dead
    /// seat's subscription and its work-watch hold until the connection
    /// closed.
    #[tokio::test]
    async fn an_unsubscribe_gives_back_every_entry_the_server_holds() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        socket.subscribe(json!("home"), false, false);
        socket.subscribe(json!("home"), false, false);
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;

        socket.unsubscribe(json!("home"));
        let _ = stub.heard_until(|v| v["kind"] == "unsubscribe").await;
        socket.unsubscribe(json!("home"));
        let _ = stub.heard_until(|v| v["kind"] == "unsubscribe").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stub.heard.recv()).await.is_err(),
            "a third unsubscribe went for a subject held twice",
        );
    }

    /// A subject two callers hold is asked for twice on a reconnect, and one
    /// unsubscribe does not drop the entry the other caller still needs.
    /// **The count is asserted exactly** - one re-ask where two were held is
    /// the server reading a seat as half-watched (`socket.test.ts` counts
    /// frames the same way).
    #[tokio::test]
    async fn a_subject_two_callers_hold_is_asked_for_twice() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        socket.subscribe(json!("home"), true, false);
        socket.subscribe(json!("home"), true, false);
        // Both went out, in order.
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;
        // The first drop: BOTH open subscriptions are re-asked.
        stub.kill.send(()).expect("the stub drops its connections");
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;
        let second = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(second["what"], json!("home"), "the second ask is the same subject");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stub.heard.recv()).await.is_err(),
            "exactly two, not three: the count is what the server was told"
        );
        // One caller leaves: the entry survives with a count of one.
        socket.unsubscribe(json!("home"));
        let _ = stub.heard_until(|v| v["kind"] == "unsubscribe").await;
        stub.kill.send(()).expect("the stub drops its connections");
        let re_asked = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(re_asked["what"], json!("home"), "the survivor is re-asked");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stub.heard.recv()).await.is_err(),
            "and only once"
        );
        // The second caller leaves: now the entry goes with it, and the next
        // reconnect asks for nothing.
        socket.unsubscribe(json!("home"));
        let _ = stub.heard_until(|v| v["kind"] == "unsubscribe").await;
        stub.kill.send(()).expect("the stub drops its connections");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stub.heard.recv()).await.is_err(),
            "the last unsubscribe dropped the entry, so nothing is re-asked"
        );
    }

    /// **A subscribe made while the socket is down is asked ONCE at the
    /// connect.** The disconnected wait holds it and the reconnect's re-ask
    /// sends it - queuing its key in both places left a phantom outstanding
    /// entry, and a later refusal would land on the phantom while the real
    /// seat kept reading as loading.
    #[tokio::test]
    async fn a_subscribe_made_while_down_is_asked_once_at_the_connect() {
        let mut stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        stub.kill.send(()).expect("the stub drops its connections");
        // The drop is seen, and the retry window is where the subscribe
        // lands - "made while down" as the bridge can really meet it.
        loop {
            if let SocketEvent::Status(Status::Connecting) = next_event(&mut rx).await {
                break;
            }
        }
        socket.subscribe(json!("home"), true, false);
        // The reconnect's re-ask is the ONE frame: the hold queued the
        // subscription, not an outstanding key as well.
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), stub.heard.recv()).await.is_err(),
            "one frame, not one from the hold and one from the re-ask"
        );
        // The snapshot answers it; the refusal that follows has nothing to
        // attribute to and must NOT read as a refused subscribe.
        stub.say
            .send(json!({ "kind": "snapshot", "subject": "home", "data": {} }))
            .expect("the stub answers");
        let _ = event_of(&mut rx, "message").await;
        stub.say
            .send(json!({ "kind": "error", "what": "subscribe", "why": "nothing outstanding" }))
            .expect("the stub refuses");
        let refused = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                if let SocketEvent::Refused { .. } = next_event(&mut rx).await {
                    return true;
                }
            }
        })
        .await;
        assert!(refused.is_err(), "no subscribe was outstanding, so nothing is attributed");
    }

    /// A close of our own making fails what a command was holding with ITS
    /// wording, not the drop's: `socket.ts` tells the two apart, and a
    /// caller that reads "the socket dropped" learns the wrong thing.
    #[tokio::test]
    async fn an_own_close_says_the_connection_was_closed() {
        let mut stub = stub().await;
        let (socket, _rx) = connected(&stub).await;
        // A reply-carrying command: the answer rides a reply, so a close
        // that beats it is what the caller reads.
        let waiting = tokio::spawn({
            let socket = socket.clone();
            async move { socket.dispatch(json!({ "spawn_worker": {} }), true).await }
        });
        // **The frame really on the wire** before the close lands on it -
        // awaiting the stub's own read, not a sleep whose margin is a coin
        // flip under load.
        let _ = stub.heard_until(|v| v["kind"] == "command").await;
        socket.close();
        match waiting.await.expect("the task joins") {
            Err(why) => assert_eq!(why, "the connection was closed"),
            Ok(_) => panic!("a command whose reply died is answered with the reason"),
        }
    }

    /// **One snapshot answers ONE of a subject's asks**, not all of them: a
    /// subject two callers hold has two outstanding keys, and a refusal
    /// still finds the second after the first is answered.
    #[tokio::test]
    async fn one_snapshot_answers_one_of_a_subjects_asks() {
        let mut stub = stub().await;
        let (socket, mut rx) = connected(&stub).await;
        socket.subscribe(json!("home"), true, false);
        socket.subscribe(json!("home"), true, false);
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;
        let _ = stub.heard_until(|v| v["kind"] == "subscribe").await;
        stub.say
            .send(json!({ "kind": "snapshot", "subject": "home", "data": {} }))
            .expect("the answer");
        let _ = event_of(&mut rx, "message").await;
        // The second ask is still owed, so a refusal lands on it...
        stub.say
            .send(json!({ "kind": "error", "what": "subscribe", "why": "still owed" }))
            .expect("the refusal");
        let refused = event_of(&mut rx, "refused").await;
        let SocketEvent::Refused { key, .. } = refused else { unreachable!("filtered") };
        assert_eq!(key, "home", "the second ask was still outstanding");
        // ...and the one after that has nothing left to attribute to.
        stub.say
            .send(json!({ "kind": "error", "what": "subscribe", "why": "nothing" }))
            .expect("the refusal");
        let seen = event_of(&mut rx, "message").await;
        let SocketEvent::Message { value, .. } = seen else { unreachable!("filtered") };
        assert_eq!(value["what"], "subscribe", "nothing outstanding reads as a plain frame");
    }

    /// **A reply the server never sends is not a caller parked forever.** An
    /// older server answers a command it does not know with an `error` frame
    /// that names no reply, so nothing resolves the pending oneshot; the
    /// dispatch gives up on its own bound instead of waiting for the socket
    /// to drop.
    #[tokio::test]
    async fn a_dispatch_whose_reply_never_comes_gives_up_on_its_bound() {
        let mut stub = stub().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let socket = Socket::open_with_reply_bound(
            stub.url.clone(),
            tx,
            Arc::new(|_| {}),
            Arc::new(|_| {}),
            Duration::from_millis(50),
        );
        loop {
            if let SocketEvent::Status(Status::Open) = next_event(&mut rx).await {
                break;
            }
        }
        let waiting = tokio::spawn({
            let socket = socket.clone();
            async move {
                socket
                    .dispatch(
                        json!({ "spawn_worker": { "project_key": "proj", "label": "w1" } }),
                        true,
                    )
                    .await
            }
        });
        let _ = stub.heard_until(|v| v["kind"] == "command").await;
        // The version-skew refusal, exactly as an older server writes it: it
        // names no reply, because the frame it could not read named none to it.
        stub.say
            .send(json!({
                "kind": "error",
                "what": "client_message",
                "why": "that is not a message this server knows",
                "seat": null,
                "dispatch_id": null,
            }))
            .expect("the refusal");
        let why = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("the dispatch gives up on its own bound")
            .expect("the task joins")
            .expect_err("no reply was sent");
        assert!(why.contains("did not answer"), "{why}");
    }

    /// **A despawn is exempt from the bound.** Its server path removes a
    /// worktree and reaps a branch before it answers, and that removal can
    /// run for minutes (#1922 measured 5-23 s, #1113 minutes); a bound would
    /// report a failure while the server is still deleting, and the retry
    /// would meet a tree that is going away.
    #[tokio::test]
    async fn a_despawn_whose_reply_never_comes_is_not_bounded() {
        let mut stub = stub().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let socket = Socket::open_with_reply_bound(
            stub.url.clone(),
            tx,
            Arc::new(|_| {}),
            Arc::new(|_| {}),
            Duration::from_millis(50),
        );
        loop {
            if let SocketEvent::Status(Status::Open) = next_event(&mut rx).await {
                break;
            }
        }
        let mut waiting = tokio::spawn({
            let socket = socket.clone();
            async move {
                socket
                    .dispatch(
                        json!({ "despawn_worker": { "project_key": "proj", "label": "w1" } }),
                        true,
                    )
                    .await
            }
        });
        let sent = stub.heard_until(|v| v["kind"] == "command").await;
        let id = sent["reply_to"].as_u64().expect("a reply id");
        // Several times the injected bound: the dispatch must still be
        // waiting, because a despawn has no honest finite bound.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut waiting).await.is_err(),
            "the despawn waits past the reply bound"
        );
        // And its reply still finds the caller.
        stub.say
            .send(json!({ "kind": "reply", "reply_to": id, "body": "gone" }))
            .expect("the reply");
        let body = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("the reply resolves the dispatch")
            .expect("the task joins")
            .expect("the dispatch resolves");
        assert_eq!(body, Some(json!("gone")));
    }

    /// Answer every probe the client sends, from a task of its own: a stub
    /// that reads and replies is what a live link looks like from here.
    fn answer_probes(stub: &Stub) -> tokio::task::JoinHandle<()> {
        let mut spoken = stub.spoken.subscribe();
        let say = stub.say.clone();
        tokio::spawn(async move {
            while let Ok(frame) = spoken.recv().await {
                if frame.get("kind").and_then(Value::as_str) == Some("ping") {
                    let _ = say.send(json!({ "kind": "pong" }));
                }
            }
        })
    }

    /// **A probe nobody answers ends the connection, and the retry redials.**
    /// A half-open socket gives no other evidence: the stub accepts and reads
    /// but never answers, which is exactly what a peer behind a dead link
    /// looks like from here.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_probe_drops_the_connection_and_redials() {
        let mut stub = stub().await;
        let (_socket, mut rx) = connected_paused(&stub).await;

        // The quiet earns a probe, and the probe is watched going out before
        // the link is called dead.
        tokio::time::advance(LIVENESS_EVERY).await;
        let probe = stub.heard_until_paused(|frame| frame["kind"] == "ping").await;
        assert_eq!(probe["kind"], "ping", "the quiet is what earns the probe");

        // Nobody answers it, so the deadline is the evidence.
        tokio::time::advance(LIVENESS_DEAD).await;
        loop {
            match next_event_paused(&mut rx, "the unanswered probe reports the drop").await {
                SocketEvent::Status(Status::Connecting) => break,
                SocketEvent::Status(Status::Open) => panic!("a live link was called dead"),
                _ => {}
            }
        }

        // And the drop path's retry redials rather than sitting the link out.
        tokio::time::advance(Duration::from_millis(RETRY_MS)).await;
        loop {
            if let SocketEvent::Status(Status::Open) =
                next_event_paused(&mut rx, "the retry dials again").await
            {
                break;
            }
        }
    }

    /// **An answered probe keeps the link open across spell after spell.**
    #[tokio::test(start_paused = true)]
    async fn an_answered_probe_keeps_the_connection_open() {
        let stub = stub().await;
        answer_probes(&stub);
        let (_socket, mut rx) = connected_paused(&stub).await;

        for _ in 0..3 {
            tokio::time::advance(LIVENESS_EVERY).await;
            loop {
                match next_event_paused(&mut rx, "the answered probe keeps the link open").await {
                    SocketEvent::Message { value, .. } => {
                        assert_eq!(value["kind"], "pong", "the probe is answered");
                        break;
                    }
                    SocketEvent::Status(Status::Connecting) => {
                        panic!("an answered probe was called dead");
                    }
                    _ => {}
                }
            }
            // The deadline then passes with the answer already read.
            tokio::time::advance(LIVENESS_DEAD).await;
        }
    }

    /// **A dial that never completes is abandoned at its bound and retried.**
    /// A peer that accepts TCP and never finishes the handshake would
    /// otherwise strand the retry loop on one dial for ever, which is this
    /// cut's failure wearing a second face.
    #[tokio::test(start_paused = true)]
    async fn a_dial_that_never_completes_is_abandoned_at_its_bound_and_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let addr = listener.local_addr().expect("the bound address");
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Held open, never answered: the handshake pends for ever.
                tokio::spawn(async move {
                    let _held = stream;
                    std::future::pending::<()>().await;
                });
            }
        });

        let (tx, _rx) = mpsc::unbounded_channel();
        let _socket = Socket::open(format!("ws://{addr}/socket"), tx);
        let real = std::time::Instant::now() + PAUSED_WAIT;
        while accepted.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            assert!(std::time::Instant::now() < real, "the dial reached the peer");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // The bound passes with no handshake, and the retry dials again.
        tokio::time::advance(DIAL_BOUND).await;
        tokio::time::advance(Duration::from_millis(RETRY_MS)).await;
        let real = std::time::Instant::now() + PAUSED_WAIT;
        while accepted.load(std::sync::atomic::Ordering::Relaxed) < 2 {
            assert!(std::time::Instant::now() < real, "the abandoned dial is retried");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// **An expired deadline dies with the socket that earned it.** Every
    /// probe is answered, but a second later than the bound - so the first
    /// connection is called dead, and the answer that lands afterwards
    /// belongs to a socket nobody is reading. The redial's connection is a
    /// fresh lifetime, and the stale deadline must not reap it at once.
    #[tokio::test(start_paused = true)]
    async fn an_expired_deadline_dies_with_the_socket_that_earned_it() {
        let stub = stub().await;
        let mut spoken = stub.spoken.subscribe();
        let say = stub.say.clone();
        tokio::spawn(async move {
            while let Ok(frame) = spoken.recv().await {
                if frame.get("kind").and_then(Value::as_str) == Some("ping") {
                    let say = say.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(LIVENESS_DEAD + Duration::from_secs(1)).await;
                        let _ = say.send(json!({ "kind": "pong" }));
                    });
                }
            }
        });

        let (_socket, mut rx) = connected_paused(&stub).await;
        tokio::time::advance(LIVENESS_EVERY).await;
        tokio::time::advance(LIVENESS_DEAD).await;
        loop {
            if let SocketEvent::Status(Status::Connecting) =
                next_event_paused(&mut rx, "the expired deadline ends its own socket").await
            {
                break;
            }
        }

        tokio::time::advance(Duration::from_millis(RETRY_MS)).await;
        loop {
            if let SocketEvent::Status(Status::Open) =
                next_event_paused(&mut rx, "the redial opens a fresh connection").await
            {
                break;
            }
        }
        // The late answer lands now, and a carried-over deadline would have
        // reaped the fresh connection the moment it opened.
        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(100)).await;
            while let Ok(event) = rx.try_recv() {
                assert!(
                    !matches!(event, SocketEvent::Status(Status::Connecting)),
                    "a replaced socket's deadline reaped the new connection",
                );
            }
        }
    }

    /// **A probe the page asks for goes out at once**, rather than waiting
    /// out the quiet: a page returning from the background asks because its
    /// own timers slept through the spell.
    #[tokio::test(start_paused = true)]
    async fn a_probe_the_page_asks_for_goes_out_at_once() {
        let mut stub = stub().await;
        let (socket, _rx) = connected_paused(&stub).await;
        // The page asks only a server that has said it speaks the probe.
        stub.say
            .send(json!({ "kind": "greeting", "version": 20, "settings": {} }))
            .expect("the greeting lands");
        settle(200).await;
        let asked = tokio::time::Instant::now();
        socket.ping();
        let probe = stub.heard_until_paused(|frame| frame["kind"] == "ping").await;
        assert_eq!(probe["kind"], "ping", "the page's ask is the probe that went");
        // **Out of the quiet's reach.** The wait creeps the paused clock, so
        // a dropped ask would be answered by the 15 s beat instead; a probe
        // inside the spell is the page's own.
        assert!(
            asked.elapsed() < LIVENESS_EVERY,
            "the probe arrived only after the quiet earned one, so the page's ask was dropped",
        );
    }

    /// **A pre-liveness server is never probed.** Its answer would be a
    /// `client_message` refusal, and this half books one on the oldest
    /// subscribe still waiting - a healthy subject marked refused every quiet
    /// spell.
    #[tokio::test(start_paused = true)]
    async fn an_older_server_is_not_probed() {
        let mut stub = stub().await;
        let (socket, mut rx) = connected_paused(&stub).await;
        stub.say
            .send(json!({ "kind": "greeting", "version": 19, "settings": {} }))
            .expect("the greeting lands");
        settle(200).await;

        // **The page's ask is refused by the same greeting.** The timed beat
        // is only half the rule; a return asks through this arm.
        socket.ping();
        settle(200).await;
        if let Ok(frame) = stub.heard.try_recv() {
            panic!("a page ask went to a pre-liveness server: {frame}");
        }

        // The first beat sees the greeting's own bytes as progress and
        // resets; the SECOND is a whole quiet spell with nothing, which is
        // where a probe would be earned - and refused.
        tokio::time::advance(LIVENESS_EVERY).await;
        settle(200).await;
        tokio::time::advance(LIVENESS_EVERY).await;
        settle(200).await;
        tokio::time::advance(LIVENESS_DEAD).await;
        settle(200).await;

        if let Ok(frame) = stub.heard.try_recv() {
            panic!("a pre-liveness server was probed: {frame}");
        }
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, SocketEvent::Status(Status::Connecting)),
                "an older server's silence reaped it",
            );
        }
    }

    /// **The page's ask waits for a greeting.** A return can land before the
    /// greeting is read, and a pre-liveness server's refusal would be booked
    /// on a subscribe.
    #[tokio::test(start_paused = true)]
    async fn a_page_probe_before_the_greeting_is_not_sent() {
        let mut stub = stub().await;
        let (socket, _rx) = connected_paused(&stub).await;
        socket.ping();
        settle(200).await;
        if let Ok(frame) = stub.heard.try_recv() {
            panic!("a probe went before the greeting was read: {frame}");
        }
    }

    /// **Bytes that predate the page's ask are not its answer.** The counter
    /// is snapshotted with the ask, so only progress after it clears the
    /// probe - otherwise a frame that arrived earlier would satisfy the
    /// question at the next beat.
    #[tokio::test(start_paused = true)]
    async fn pre_ask_bytes_do_not_answer_a_page_probe() {
        let stub = stub().await;
        let (socket, mut rx) = connected_paused(&stub).await;
        stub.say
            .send(json!({ "kind": "greeting", "version": 20, "settings": {} }))
            .expect("the greeting lands");
        stub.say.send(json!({ "kind": "probe", "pad": "x" })).expect("a frame lands");
        settle(300).await;
        // The frames' bytes are in, and the client has read them.
        while rx.try_recv().is_ok() {}

        socket.ping();
        settle(200).await;
        tokio::time::advance(LIVENESS_DEAD).await;
        settle(200).await;

        let mut dropped = false;
        while let Ok(event) = rx.try_recv() {
            dropped |= matches!(event, SocketEvent::Status(Status::Connecting));
        }
        assert!(
            dropped,
            "bytes that predate the probe answered it, so a dead link survived the deadline",
        );
    }

    /// **A second page ask does not push the deadline out.** The ones already
    /// out stands, as the page's own deadline does.
    #[tokio::test(start_paused = true)]
    async fn a_second_page_probe_does_not_push_the_deadline() {
        let stub = stub().await;
        let (socket, mut rx) = connected_paused(&stub).await;
        stub.say
            .send(json!({ "kind": "greeting", "version": 20, "settings": {} }))
            .expect("the greeting lands");
        settle(200).await;

        socket.ping();
        settle(100).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        settle(100).await;
        socket.ping();
        settle(100).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        settle(200).await;

        let mut dropped = false;
        while let Ok(event) = rx.try_recv() {
            dropped |= matches!(event, SocketEvent::Status(Status::Connecting));
        }
        assert!(dropped, "the second ask pushed the first deadline out");
    }

    /// **A pong that answered the first ask keeps it answered after a second
    /// ask.** The page's probe returns early while one is out; the shell must
    /// not re-snapshot the counter, or the answer would stop counting as
    /// progress and the first deadline would reap a live link.
    #[tokio::test(start_paused = true)]
    async fn a_pong_answering_the_first_ask_survives_a_second_ask() {
        let stub = stub().await;
        let (socket, mut rx) = connected_paused(&stub).await;
        stub.say
            .send(json!({ "kind": "greeting", "version": 20, "settings": {} }))
            .expect("the greeting lands");
        settle(200).await;

        socket.ping();
        settle(200).await;
        stub.say.send(json!({ "kind": "pong" })).expect("the answer lands");
        settle(300).await;

        socket.ping();
        settle(200).await;
        tokio::time::advance(LIVENESS_DEAD).await;
        settle(200).await;

        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, SocketEvent::Status(Status::Connecting)),
                "the second ask re-snapshotted the counter, so the first answer stopped counting",
            );
        }
    }

    /// **The shipping liveness pair is the value the book states.** What the
    /// book tells a client author to expect, and the floor the server's own
    /// write bound sets for the peer that is slow rather than dead.
    #[test]
    fn the_shipping_liveness_pair_is_the_documented_pair() {
        assert_eq!(LIVENESS_EVERY, Duration::from_secs(15), "a probe after 15 s of quiet");
        assert_eq!(LIVENESS_DEAD, Duration::from_secs(10), "and 10 s without a byte in answer");
    }

    /// A stub that speaks one text frame in two halves: the first written at
    /// once, the rest only when the test says so. Nothing else is answered.
    ///
    /// **The bytes are a valid frame in progress**, which is the state a
    /// message-level deadline mistakes for silence.
    async fn partial_frame_stub(payload_len: usize, first: usize) -> (String, mpsc::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let addr = listener.local_addr().expect("the bound address");
        let (finish, mut finished) = mpsc::channel::<()>(1);
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else { return };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
            let mut payload = String::with_capacity(payload_len + 40);
            payload.push_str("{\"kind\":\"greeting\",\"pad\":\"");
            payload.push_str(&"a".repeat(payload_len));
            payload.push_str("\"}");
            let bytes = payload.as_bytes();
            let mut frame = Vec::with_capacity(bytes.len() + 10);
            // FIN | text, and a 64-bit length: a server frame is unmasked.
            frame.push(0x81);
            frame.push(0x7f);
            frame.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            frame.extend_from_slice(bytes);
            let socket = ws.get_mut();
            if socket.write_all(&frame[..first]).await.is_err() {
                return;
            }
            let _ = socket.flush().await;
            if finished.recv().await.is_none() {
                return;
            }
            if socket.write_all(&frame[first..]).await.is_err() {
                return;
            }
            let _ = socket.flush().await;
            std::future::pending::<()>().await;
        });
        (format!("ws://{addr}/socket"), finish)
    }

    /// **A frame still arriving is not silence.** tungstenite hands a message
    /// over only once the frame is complete, and the server may spend its
    /// whole write bound on one; a message-level deadline would reap a live
    /// peer mid-frame, redial, and re-ask the same record for ever. The byte
    /// counter is the evidence, so the partial frame keeps the link alive and
    /// the message then completes on the same connection.
    #[tokio::test(start_paused = true)]
    async fn a_slow_frame_is_not_called_dead() {
        let (url, finish) = partial_frame_stub(512 * 1024, 64 * 1024).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _socket = Socket::open(url, tx);
        loop {
            if let SocketEvent::Status(Status::Open) =
                next_event_paused(&mut rx, "the socket opened").await
            {
                break;
            }
        }
        // The first half is on the wire; give the runtime real time to read
        // it, which is what advances the counter. The wait is real rather
        // than virtual because the socket is, and a loaded runner has held
        // I/O-adjacent work for seconds.
        settle(1_000).await;

        // Past a quiet spell, then past the deadline that follows it. The
        // settle between them is what makes the second advance reach a timer
        // the first one armed: a deadline counting messages reaps on the
        // second, while the byte counter made the first a no-op.
        tokio::time::advance(LIVENESS_EVERY).await;
        settle(200).await;
        tokio::time::advance(LIVENESS_DEAD).await;
        settle(200).await;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, SocketEvent::Status(Status::Connecting)),
                "a frame still arriving was called silence",
            );
        }

        // The rest of the frame lands, and the message finishes on a live link.
        let _ = finish.send(()).await;
        loop {
            match next_event_paused(&mut rx, "the slow frame completes").await {
                SocketEvent::Message { value, .. } => {
                    assert_eq!(value["kind"], "greeting", "the frame completed whole");
                    break;
                }
                SocketEvent::Status(Status::Connecting) => {
                    panic!("the slow frame's connection was reaped");
                }
                _ => {}
            }
        }
    }
}
