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
//! a drop. One kind is consumed here alone: `reply` is `dispatch`'s own
//! correlated answer (minted and matched in `socket.rs`). The browser's state
//! frames are forwarded AND acted on - the viewer takes the wire id and the
//! failure, the page's surface store takes the history - because the surface
//! is drawn by this process and its bar by the page.
//!
//! **Frames pause while the page is away, and catch up from the records.**
//! The page heartbeats; one older than `FRESH_MS` means a suspended webview,
//! and live emission stops, so a parked page cannot grow an unbounded event
//! backlog. Nothing is lost with it: **every suppressed frame except a
//! seat's chat is HELD in a bounded ring** - a released seat, a dock's
//! ending, a dictation's close, a prompt's queued row, an error - and the
//! first heartbeat after the spell answers with the ring
//! itself, in order, and then with the records: a snapshot per seat, which
//! is what a record is, and that seat's newest page, which is the answer
//! its `more` with no `before` would have carried. Only `chat_appended`
//! updates are left out of the ring, because the record's conversation
//! rebuilds those. A subject that is not a seat is re-read on the reconcile
//! rather than handed back: the home roster is copied here for the ask
//! rule's own fold, not as a record to restore.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use forge_primitives::SessionSlot;
use serde_json::{Value, json};
use tauri::Emitter as _;
use tauri::Manager as _;
use tokio::sync::mpsc;

use crate::perf;
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
/// draws from the same stream it always did. The other arm is the
/// connection's own fact, which never rode the frame stream: where it is in
/// its life.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// The connection's state moved; the page re-reads it with `client_state`.
    Connection,
    /// A `ServerMessage`, as itself.
    Inbound(Value),
    /// A forwarded snapshot's own frame facts: its raw length and the parse
    /// this half took, for the page's perf log (#2513).
    Framed { subject: Value, bytes: usize, parse_ms: f64 },
    /// A subscribe frame finished writing in this half, with its wait
    /// (#2513).
    Written { key: String, declared: bool, waited_ms: f64 },
    /// A subscribe was refused, with the core's own words.
    Refused { key: String, why: String },
    /// One frame of the desktop's own capture, for the page to stream.
    Mic(Vec<u8>),
}

/// One subject the page watches: its own subject, how many page readers hold
/// it, and the declaration they asked with.
struct Watch {
    what: Value,
    readers: usize,
    answering: bool,
    /// Whether the last read asked for this subject was the review page's
    /// own. Follows the page holding the seat, so a review open that drops
    /// comes back as the read it was rather than as an ordinary record.
    review: bool,
}

/// A live takeover's viewer, as the attach asked for it.
///
/// **The server drops a viewer with the socket it attached on**, so a
/// reconnect has to ask again or the surface keeps a picture no gesture can
/// reach. Kept here rather than in the page: the connection's own life is
/// this half's to watch.
#[derive(Clone)]
struct Attach {
    seat: Value,
    /// The tab the attach landed on, which is what a reconnect asks for.
    tab: Option<u16>,
}

struct State {
    records: Records,
    held: HashMap<String, Watch>,
    /// The held keys in first-asked order. `held` is a map, and a reconnect
    /// must re-ask the FIRST subject subscribed, as `socket.ts`'s
    /// `held.values()` (insertion-ordered) does - a HashMap's own order is
    /// arbitrary and would re-ask a different subject per run.
    order: Vec<String>,
    socket: Option<Socket>,
    /// Which socket is the CURRENT one, so a replaced socket's late events -
    /// a `Status::Open` from a watcher still retrying behind a fresh
    /// connection - are ignored rather than counted.
    socket_id: u64,
    status: &'static str,
    /// How many sockets have OPENED on this bridge.
    ///
    /// **The page's proof that the connection it believes open is the one
    /// still there.** A webview suspended across a drop and its reconnect
    /// wakes to a socket that is already `open` again: every queued
    /// connection notice re-reads the CURRENT state, so the page never sees
    /// the `connecting` and would keep a frozen record for ever. A greeting
    /// is not that proof - the first socket's greeting is delivered twice
    /// and two reconnects to one server carry the same value - so the shell
    /// counts the sockets themselves and the page compares.
    ///
    /// Bumped on every `Status::Open`, not in [`Bridge::connect`]: the
    /// socket reconnects itself, and its own retries never pass through
    /// `connect`.
    generation: u64,
    greeting: Option<Value>,
    /// When the page last heartbeated, and how many frames went unforwarded
    /// while it was presumed parked. **Every suppressed frame except a
    /// seat's chat is HELD, not dropped** (rule 25): the ring carries a
    /// released seat, a dock's ending, a dictation's close, a prompt's
    /// queued row and an error across the spell, and the
    /// reconcile flushes it first. Only `chat_appended` updates are left
    /// out, because the record's conversation rebuilds those.
    last_beat: Option<Instant>,
    suppressed: u64,
    parked: VecDeque<Value>,
    /// Seats a REPLACES frame has asked a fresh read for and no snapshot has
    /// answered yet: a second frame while one is out coalesces into it
    /// rather than asking again. Cleared by the answer, a refusal, or a new
    /// connection.
    refreshing: HashSet<String>,
    /// When the page last asked for a subject, so the reconcile does not ask
    /// for the same one again inside the heartbeat window.
    ///
    /// **One return can name two reads.** The page's own return re-read asks
    /// first; the first heartbeat after a lock is always stale (the page's
    /// timers slept), and the reconcile would ask for every held non-seat
    /// subject again - the home encode walks every project's working tree, on
    /// the platform this cut exists for.
    refreshed: HashMap<String, Instant>,
    /// The takeover's live viewer, re-attached on every reconnect while it
    /// is set; a lower clears it.
    attach: Option<Attach>,
    /// Which attach (or lower) is current. A re-attach that lands after a
    /// later lower or a second attach is stale, and takes itself back.
    attach_generation: u64,
    /// The profile the viewed page belongs to, from the last `browser_view`:
    /// **the perf marks name it**, so two profiles' tab 2s are not one page
    /// in the log (#2645).
    view_profile: Option<String>,
    /// The tab a gesture is waiting for a frame from: the round trip's other
    /// end, spent by the first frame on that tab (#2645).
    awaiting_paint: Option<u16>,
    /// The seat the page has on screen, as the last `showing` said.
    shown: Option<SessionSlot>,
    /// The asks this half has announced, over the home reads it sees.
    asks: crate::asks::Asks,
}

/// Where a bridge's ask notices go.
///
/// **A seam for the tests, not a second delivery**: the shipping type is
/// `PlatformAlerts`, which hands each notice to the notification thread.
trait Alerts: Send + Sync {
    fn raise(&self, seat: &SessionSlot, kind: crate::asks::AskKind);
}

/// The shell's delivery: one job per notice on the notification thread.
struct PlatformAlerts {
    app: tauri::AppHandle,
}

impl Alerts for PlatformAlerts {
    fn raise(&self, seat: &SessionSlot, kind: crate::asks::AskKind) {
        crate::notify::raise(&self.app, seat, kind);
    }
}

/// The app half, as the Tauri layer drives it.
pub struct Bridge {
    state: Mutex<State>,
    /// The app, for the one sink the socket needs: relayed browser frames
    /// land in the viewer, which draws on the main thread. `None` in a test
    /// that never draws one.
    app: Option<tauri::AppHandle>,
    events: mpsc::UnboundedSender<ClientEvent>,
    /// Where ask notices go; `None` in a test that does not record them.
    alerts: Option<Arc<dyn Alerts>>,
    /// Whether the person is looking at the app, as the last focus or
    /// life-cycle event said. **Seeded true**: the window is what activated
    /// the app, and every later change arrives as an event.
    attention: AtomicBool,
    /// The wire sequence for the viewer's input, minted here so the overlay
    /// and the page share one ordered stream.
    input_seq: AtomicU64,
    /// **One picker at a time.** A newer ask while a panel is open would
    /// otherwise open a second panel for a dialog the first is already
    /// answering; the open panel's answer follows the newest ask instead.
    picking: std::sync::atomic::AtomicBool,
    /// The ask the open panel's answer belongs to.
    live_file: AtomicU64,
    /// When a browser refusal was last logged, **on the monotonic clock**:
    /// a backwards system clock step would otherwise silence the record
    /// until it caught up. A pointer move is a stream - a surface with no
    /// role is refused one per move - and the first of the burst is the
    /// record, not every frame.
    refusal_logged: Mutex<Option<Instant>>,
}

/// Clears the picker flag however the picker ends, a panic included.
struct Picking<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Picking<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// The next socket number for a bridge, unique for the process's life.
///
/// Process-wide rather than per bridge so two bridges in one process - a
/// test's, a scratch instance beside the real one - cannot hand the same
/// number to different sockets.
fn mint_socket_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Bridge {
    pub fn new(
        app: Option<tauri::AppHandle>,
        events: mpsc::UnboundedSender<ClientEvent>,
    ) -> Arc<Self> {
        let alerts = app
            .as_ref()
            .map(|app| Arc::new(PlatformAlerts { app: app.clone() }) as Arc<dyn Alerts>);
        Self::with_alerts(app, events, alerts)
    }

    /// The one constructor, so a test can hand in its own recorder.
    fn with_alerts(
        app: Option<tauri::AppHandle>,
        events: mpsc::UnboundedSender<ClientEvent>,
        alerts: Option<Arc<dyn Alerts>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                records: Records::new(),
                held: HashMap::new(),
                order: Vec::new(),
                socket: None,
                socket_id: 0,
                status: "connecting",
                generation: 0,
                greeting: None,
                last_beat: None,
                suppressed: 0,
                parked: VecDeque::new(),
                refreshing: HashSet::new(),
                refreshed: HashMap::new(),
                attach: None,
                attach_generation: 0,
                view_profile: None,
                awaiting_paint: None,
                shown: None,
                asks: crate::asks::Asks::default(),
            }),
            app,
            events,
            alerts,
            attention: AtomicBool::new(true),
            input_seq: AtomicU64::new(0),
            picking: std::sync::atomic::AtomicBool::new(false),
            live_file: AtomicU64::new(0),
            refusal_logged: Mutex::new(None),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: ClientEvent) {
        let _ = self.events.send(event);
    }

    /// The connection as it stands: a fresh clone per call, so a reconnect
    /// between calls is simply picked up next time.
    pub(crate) fn socket(&self) -> Option<Socket> {
        self.lock().socket.clone()
    }

    /// Attach this connection to a seat's browser tab as a viewer, answering
    /// the profile and tab the server attached: a refused attach is the
    /// surface's sentence. **The answer is the pair the surface's Done
    /// lowers**, since a raise that named no tab resolved to the asking
    /// seat's own current one.
    pub async fn browser_view(&self, seat: Value, tab: Option<u16>) -> Result<Value, String> {
        let socket = self.socket().ok_or_else(|| "the socket is not open".to_owned())?;
        let attached = match socket.browser_view(seat.clone(), tab).await {
            Ok(attached) => attached,
            Err(why) => {
                // **A refused attach is the surface's failure, not the last
                // tab's picture left standing over it.** The old attachment
                // is dropped so a reconnect does not ask for the dead tab,
                // and the overlay is lowered under the sentence.
                let mut state = self.lock();
                state.attach = None;
                state.attach_generation = state.attach_generation.wrapping_add(1);
                drop(state);
                if let Some(app) = &self.app {
                    crate::browser::lost(app);
                }
                self.emit(ClientEvent::Inbound(
                    json!({ "kind": "browser_surface_failed", "reason": why }),
                ));
                return Err(why);
            }
        };
        // **A landed attach is the takeover's to keep**: the next reconnect
        // asks for it again, so the surface never outlives its viewer. The
        // resolved tab is kept, not the request's own, so a reconnect asks
        // for the very tab that came up.
        let resolved =
            attached.get("tab").and_then(Value::as_u64).and_then(|tab| u16::try_from(tab).ok());
        let mut state = self.lock();
        state.attach = Some(Attach { seat, tab: resolved });
        state.attach_generation = state.attach_generation.wrapping_add(1);
        Ok(attached)
    }

    /// The takeover lowered: stop viewing the profile, so the server's idle
    /// clock can release it.
    ///
    /// **A lower with nothing attached sends nothing.** The page lowers on
    /// several paths (its own close, its unmount, the hardware Back) and
    /// more than one can fire for one takeover; each one used to tell the
    /// server to detach an attachment that was already gone.
    pub fn browser_detach(&self) {
        let held = {
            let mut state = self.lock();
            let held = take_attach(&mut state);
            state.attach_generation = state.attach_generation.wrapping_add(1);
            held
        };
        if held.is_some()
            && let Some(socket) = self.socket()
        {
            socket.browser_detach();
        }
    }

    /// Open this machine's picker for a page's file chooser and answer it.
    ///
    /// **A dismiss is an answer too**: the host is told with no paths, which
    /// cancels CEF's dialog rather than leaving the page waiting on one.
    async fn pick_files(self: &Arc<Self>, app: &tauri::AppHandle, multiple: bool) {
        use tauri_plugin_dialog::DialogExt as _;

        // **The flag is held for the panel's whole life**, so a newer ask
        // cannot stack a second panel on top of this one.
        let _picking = Picking(&self.picking);
        let opening = self.live_file.load(std::sync::atomic::Ordering::Acquire);
        tauri_plugin_log::log::info!(
            "the page's file chooser opens this machine's picker (event_name \
             client_browser_file_picker, id {opening}, multiple {multiple})"
        );
        let picked = if multiple {
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.dialog().file().pick_files(move |paths| {
                let _ = tx.send(paths);
            });
            rx.await.ok().flatten()
        } else {
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.dialog().file().pick_file(move |path| {
                let _ = tx.send(path.map(|path| vec![path]));
            });
            rx.await.ok().flatten()
        };
        // **The answer belongs to the newest ask.** A second chooser raised
        // while this panel is open supersedes the one it was opened for, and
        // the host has already cancelled that older callback; answering it
        // would be refused while the live ask went unanswered. The panel is
        // the person's single chooser, so its answer follows the live id.
        let live = self.live_file.load(std::sync::atomic::Ordering::Acquire);
        let Some(paths) = picked else {
            tauri_plugin_log::log::info!(
                "the picker was dismissed (event_name client_browser_file_dismissed, id {live})"
            );
            self.answer_files(live, Vec::new());
            return;
        };
        self.answer_paths(app, live, paths).await;
    }

    /// The chosen files' bytes back under the dialog's id.
    ///
    /// **Split from the picker so a look can drive this half with a real
    /// path**: the panel's own selection is a person's click and cannot be
    /// driven on this machine.
    pub(crate) async fn answer_paths(
        self: &Arc<Self>,
        app: &tauri::AppHandle,
        id: u64,
        paths: Vec<tauri_plugin_fs::FilePath>,
    ) {
        use tauri_plugin_fs::{FsExt as _, OpenOptions};

        let cap = forge_primitives::browser::MAX_UPLOAD_BYTES;
        let mut files = Vec::new();
        let mut total = 0_u64;
        for path in paths {
            let name = Self::uploaded_name(app, &path).await;
            // **The size is checked before the bytes are read** when the path
            // gives one: a huge file is refused without loading it, and the
            // running total is checked the same way for every file.
            let known = match &path {
                tauri_plugin_fs::FilePath::Path(path) => {
                    std::fs::metadata(path).map(|meta| meta.len()).ok()
                }
                tauri_plugin_fs::FilePath::Url(_) => None,
            };
            if past_budget(files.len(), total, known) {
                tauri_plugin_log::log::info!(
                    "the chosen files are past the caps and the dialog is cancelled (event_name \
                     client_browser_file_over_cap, id {id}, name {name})"
                );
                self.answer_files(id, Vec::new());
                return;
            }
            let remaining = cap.saturating_sub(total);
            let mut options = OpenOptions::new();
            options.read(true);
            let bytes = match app.fs().open(path, options) {
                Ok(file) => match read_within(file, remaining) {
                    Ok(bytes) => bytes,
                    Err(why) => {
                        tauri_plugin_log::log::warn!(
                            "a chosen file could not be read (event_name \
                             client_browser_file_unreadable, {name}): {why}"
                        );
                        continue;
                    }
                },
                Err(why) => {
                    tauri_plugin_log::log::warn!(
                        "a chosen file could not be opened (event_name \
                         client_browser_file_unreadable, {name}): {why}"
                    );
                    continue;
                }
            };
            let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if size > remaining {
                // **One byte past the budget is the answer**: the file grew
                // past the cap after its metadata was read.
                tauri_plugin_log::log::info!(
                    "the chosen files are past the caps and the dialog is cancelled (event_name \
                     client_browser_file_over_cap, id {id}, name {name})"
                );
                self.answer_files(id, Vec::new());
                return;
            }
            total = total.saturating_add(size);
            use base64::Engine as _;
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            files.push(json!({ "name": name, "data": data }));
        }
        tauri_plugin_log::log::info!(
            "the file chooser is answered (event_name client_browser_file_answered, id {id}, \
             files {})",
            files.len()
        );
        self.answer_files(id, files);
    }

    /// The name the page must see for a chosen file.
    ///
    /// **A path keeps its basename; a URI asks Android for the document's own
    /// name**, because a content URI carries an opaque id rather than a file
    /// name - the page used to see `0-file` where the person chose a resume.
    async fn uploaded_name(_app: &tauri::AppHandle, path: &tauri_plugin_fs::FilePath) -> String {
        #[cfg(target_os = "android")]
        if let tauri_plugin_fs::FilePath::Url(url) = path
            && let Some(name) = android_uri_name(_app, url.as_str()).await
        {
            return name;
        }
        local_name(path)
    }

    /// The chosen files, back to the server under the dialog's id.
    fn answer_files(&self, id: u64, files: Vec<Value>) {
        if let Some(socket) = self.socket() {
            socket.browser_file_answer(id, Value::Array(files));
        }
    }

    /// One input event from the controlling viewer. **The sequence is
    /// minted here**, so the overlay's gestures and the page's are one
    /// ordered stream on the wire whatever sent them.
    pub fn browser_input(&self, event: forge_primitives::browser::BrowserInputEvent) {
        let seq = self.input_seq.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(socket) = self.socket() {
            // **A gesture with no socket never leaves**, so it is not marked:
            // a mark with no frame after it would read as a page that never
            // drew what was never sent.
            self.mark_browser_input(&event);
            socket.browser_input(seq, event);
        }
    }

    /// **A gesture's mark, and the frame it will wait on** (#2645): the round
    /// trip is read from here to the first frame drawn for its tab.
    fn mark_browser_input(&self, event: &forge_primitives::browser::BrowserInputEvent) {
        // **The desktop overlay is the only shell that draws the picture**:
        // everywhere else the page paints its own canvas and marks there, so
        // a shell mark beside it would write every gesture twice.
        if !cfg!(target_os = "macos") {
            return;
        }
        let Some(name) = gesture_mark(event) else { return };
        let Some(app) = self.app.as_ref() else { return };
        let (tab, profile) = {
            let mut state = self.lock();
            // **A closed connection drops the gesture**, so a mark written
            // for it would pair with no frame at all.
            if state.status != "open" {
                return;
            }
            let Some(tab) = state.attach.as_ref().and_then(|attach| attach.tab) else { return };
            state.awaiting_paint = Some(tab);
            (tab, state.view_profile.clone())
        };
        perf::note_browser_input(app, name, profile.as_deref(), tab);
    }

    /// **The frame that answers a gesture is the first drawn for its tab**
    /// (#2645). The overlay draws what the socket hands it, so the sink is
    /// where the desktop's end of the round trip is marked; the page marks
    /// its own canvas after the draw.
    fn browser_painted(&self, bytes: &[u8]) {
        // The same platform rule as the input mark: one writer per client.
        if !cfg!(target_os = "macos") {
            return;
        }
        let Some(frame) = crate::browser::split_frame(bytes) else { return };
        let Some(app) = self.app.as_ref() else { return };
        let (owed, profile) = {
            let mut state = self.lock();
            (perf::paint_owed(&mut state.awaiting_paint, frame.tab), state.view_profile.clone())
        };
        if owed {
            perf::note_browser_paint(app, profile.as_deref(), frame.tab, frame.seq);
        }
    }

    /// Take the controller role, or give it back.
    pub fn browser_control(&self, take: bool) {
        if let Some(socket) = self.socket() {
            socket.browser_control(take);
        }
    }

    /// Whether this refusal is the first of its burst on the monotonic
    /// clock, so a refused pointer move is one line rather than one per
    /// frame.
    fn refusal_is_new(&self) -> bool {
        const EVERY: Duration = Duration::from_secs(1);
        let now = Instant::now();
        let mut last = self.refusal_logged.lock().unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|last| now.duration_since(last) < EVERY) {
            return false;
        }
        *last = Some(now);
        true
    }

    /// Move through the page's history.
    pub fn browser_nav(&self, back: bool) {
        if let Some(socket) = self.socket() {
            socket.browser_nav(back);
        }
    }

    /// The controller's surface changed size: device pixels and the ratio.
    pub fn browser_resize(&self, width: u32, height: u32, scale: f64) {
        if let Some(socket) = self.socket() {
            socket.browser_resize(width, height, scale);
        }
    }

    /// The tabs of this seat's profile, for the strip's rows.
    pub async fn browser_tabs(&self, seat: Value) -> Result<Value, String> {
        let socket = self.socket().ok_or_else(|| "the socket is not open".to_owned())?;
        socket.browser_tabs(seat).await
    }

    /// Dial the server, replacing any connection this bridge had. Whatever
    /// the page already watches is re-asked on the new socket, so a
    /// reconnect from the connect screen lands the subscriptions it had.
    ///
    /// **Answers this connection's own id**, which the page hands back to
    /// [`Bridge::close`]: the page dials an address before it gives the
    /// connection it replaced back, so a close must name the connection it
    /// opened rather than "whatever the bridge holds".
    pub fn connect(self: &Arc<Self>, url: String) -> u64 {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let frames: crate::socket::FrameSink = match &self.app {
            Some(app) => {
                let app = app.clone();
                let bridge = Arc::clone(self);
                Arc::new(move |bytes: &[u8]| {
                    crate::browser::frame(&app, bytes);
                    bridge.browser_painted(bytes);
                })
            }
            None => Arc::new(|_: &[u8]| {}),
        };
        let sound: crate::socket::AudioSink = match &self.app {
            Some(app) => {
                let app = app.clone();
                Arc::new(move |bytes: &[u8]| crate::browser::audio(&app, bytes))
            }
            None => Arc::new(|_: &[u8]| {}),
        };
        let socket = Socket::open_with_frames(url, tx, frames, sound);
        // **This socket's own identity.** The old socket is closed below, but
        // its task can still deliver a queued event after the swap; the id is
        // what keeps a stale `Status::Open` from bumping the page's
        // generation or writing to the fresh connection's state.
        let socket_id = mint_socket_id();
        let wanted: Vec<(Value, bool, bool, usize)> = {
            let mut state = self.lock();
            // **The park outlives the connection it was kept across**: a
            // page away when the socket drops is still away, so the held
            // frames are handed over before the ring is emptied rather than
            // discarded for a fresh start. Emitting into a parked page's
            // queue is bounded by the ring's own cap.
            while let Some(message) = state.parked.pop_front() {
                self.emit(ClientEvent::Inbound(message));
            }
            // **The old socket ends with the swap**, and its id goes with it:
            // a watcher left retrying behind the new connection is a second
            // socket writing into one bridge.
            if let Some(previous) = state.socket.take() {
                previous.close();
            }
            state.socket = Some(socket.clone());
            state.socket_id = socket_id;
            state.status = "connecting";
            state.greeting = None;
            state.last_beat = Some(Instant::now());
            state.suppressed = 0;
            state.refreshing.clear();
            let mut wanted = Vec::with_capacity(state.order.len());
            for key in &state.order {
                if let Some(watch) = state.held.get(key) {
                    // **A seat is the page's own to re-open.** Only the shown
                    // seat re-attaches after a resume; the rest re-attach on
                    // their next click, so re-asking every visited seat here
                    // would pay a whole record each for seats nobody is
                    // looking at.
                    if !watch.what.is_string() {
                        continue;
                    }
                    wanted.push((watch.what.clone(), watch.answering, watch.review, watch.readers));
                }
            }
            wanted
        };
        for (what, answering, review, readers) in wanted {
            // Once per OPEN subscription: the server counts, and the new
            // socket starts at zero - a subject two page readers hold has to
            // be asked for twice, or the first unsubscribe silences it.
            for _ in 0..readers {
                socket.subscribe(what.clone(), answering, review);
            }
        }
        self.emit(ClientEvent::Connection);
        let bridge = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                bridge.socket_event(socket_id, event);
            }
        });
        socket_id
    }

    /// Watch one subject, as one page reader more. The wire frame is passed
    /// through per call - the server counts subscriptions, so two readers are
    /// two frames and the record's lifetime is this side's own count.
    pub fn subscribe(&self, what: Value, answering: bool, review: bool) {
        let key = records::subject_key(&what);
        if key.is_empty() {
            return;
        }
        let mut state = self.lock();
        match state.held.get_mut(&key) {
            Some(watch) => {
                watch.readers += 1;
                watch.answering |= answering;
                watch.review = review;
            }
            None => {
                state.order.push(key.clone());
                state.held.insert(key, Watch { what: what.clone(), readers: 1, answering, review });
            }
        }
        if let Some(socket) = &state.socket {
            socket.subscribe(what, answering, review);
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
            state.order.retain(|held| *held != key);
            state.records.release(&key);
        }
        if let Some(socket) = &state.socket {
            socket.unsubscribe(what.clone());
        }
    }

    /// Say which seat the page has on screen, `None` for none.
    ///
    /// **A state assertion, not an event**: the page re-sends it on every
    /// reconnect, and the server spends a seat's marks on it and never on a
    /// subscription.
    pub fn showing(&self, seat: Option<Value>) {
        let mut state = self.lock();
        state.shown = seat.as_ref().and_then(|held| serde_json::from_value(held.clone()).ok());
        if let Some(socket) = &state.socket {
            socket.showing(seat);
        }
        drop(state);
        // **Looking away is what lets a glanced-at ask be announced**, and a
        // seat switch is exactly that - so the held roster is read again
        // rather than waiting for the page's next read, which a parked page
        // never makes.
        let notices = self.asks_moved();
        self.raise(notices);
    }

    /// The seat the page has on screen, as the last `showing` said.
    pub fn shown(&self) -> Option<SessionSlot> {
        self.lock().shown.clone()
    }

    /// Ask again for a subject the page still holds, **without counting a
    /// reader**: the record and the page's store both survived the drop, and
    /// only the server's subscription went with the socket.
    pub fn relist(&self, what: Value, answering: bool, review: bool) {
        let mut state = self.lock();
        let key = records::subject_key(&what);
        let held_answering = state.held.get(&key).map(|watch| watch.answering).unwrap_or(false);
        // **The declaration is the held one, with this ask's own review
        // scope**: the role only rises while the connection lives, and the
        // review read follows the page that last held the seat.
        if let Some(watch) = state.held.get_mut(&key) {
            watch.review = review;
        }
        if let Some(socket) = &state.socket {
            socket.relist(what, held_answering || answering, review);
        }
    }

    /// Change a subject's held declaration without asking the server for
    /// anything: the scope a page holds, said so a later reconnect does not
    /// re-ask it as a review read nobody wanted.
    pub fn declare(&self, what: Value, review: bool) {
        let mut state = self.lock();
        let key = records::subject_key(&what);
        if let Some(watch) = state.held.get_mut(&key) {
            watch.review = review;
        }
        if let Some(socket) = &state.socket {
            socket.declare(what, review);
        }
    }

    /// Give a subject back entirely: every page reader's subscription and
    /// the server's entries for it.
    ///
    /// **The count released is the server's, not the page's.** After a
    /// resume the bridge may hold two readers where the wire holds one, so
    /// one unsubscribe call per reader would send a frame the server has
    /// nothing for and leave the one it does hold standing; the socket's own
    /// count is what caps the sends.
    pub fn release(&self, what: &Value) {
        let key = records::subject_key(what);
        let mut state = self.lock();
        state.held.remove(&key);
        state.order.retain(|held| *held != key);
        state.records.release(&key);
        // **The socket gives back the wire's count.** A relist raises it
        // above this side's readers, so looping the readers would leave a
        // subscription and its work-watch hold standing.
        if let Some(socket) = &state.socket {
            socket.release(what.clone());
        }
    }

    /// Ask again for a subject the page already holds: the core answers with
    /// a snapshot, which replaces the record whole. **The declaration is the
    /// held one**: a refresh with flags of its own would escalate a seat
    /// watched without them, and the core parks turns on whatever answered.
    pub fn refresh(&self, what: &Value, review: Option<bool>) {
        let mut state = self.lock();
        let key = records::subject_key(what);
        // **A refresh that states no scope keeps the held one.** The page's
        // own re-ask says which read it wants; a bound's retry for a store
        // still loading says nothing, and turning it into an ordinary record
        // would undo the review read it is retrying.
        if let Some(review) = review
            && let Some(watch) = state.held.get_mut(&key)
        {
            watch.review = review;
        }
        let (answering, held_review) = state
            .held
            .get(&key)
            .map(|watch| (watch.answering, watch.review))
            .unwrap_or((false, review.unwrap_or(false)));
        let sent = match &state.socket {
            Some(socket) => {
                records::refresh(socket, what, answering, held_review);
                true
            }
            None => false,
        };
        // Stamped only when the ask really went: a reconcile over this same
        // return must not answer for a read that never left.
        if sent {
            state.refreshed.insert(key, Instant::now());
        }
    }

    /// Probe the link now, on the page's return from the background.
    ///
    /// **The page's timers slept with it, and so did the socket's beat** -
    /// this is what asks the question again at the moment the answer
    /// matters, rather than up to a quiet spell later.
    pub fn probe(&self) {
        if let Some(socket) = &self.lock().socket {
            socket.ping();
        }
    }

    /// The home roster moved: the asks it holds for a seat the person is not
    /// looking at.
    ///
    /// **The rule is the page's** (`crate::asks`): a seat is on screen when
    /// it is the shown seat and the window has focus. Runs over the held
    /// home read, so a parked page's asking costs nothing. The caller raises
    /// what this answers, which is what lets a test see the trigger fire.
    fn asks_moved(&self) -> Vec<(SessionSlot, crate::asks::AskKind)> {
        let focused = self.looking();
        let mut state = self.lock();
        let State { records, asks, shown, .. } = &mut *state;
        records
            .read("home")
            .and_then(|held| held.ok())
            .map(|data| asks.rising(data, shown.as_ref(), focused))
            .unwrap_or_default()
    }

    /// Raise one roster read's notices where this bridge has a delivery.
    fn raise(&self, notices: Vec<(SessionSlot, crate::asks::AskKind)>) {
        let Some(alerts) = self.alerts.as_ref() else { return };
        for (seat, kind) in notices {
            alerts.raise(&seat, kind);
        }
    }

    /// **Whether the person is looking at the app**, which the rule's
    /// exemption turns on: the last focus or life-cycle event's own answer.
    fn looking(&self) -> bool {
        self.attention.load(Ordering::Relaxed)
    }

    /// Re-evaluate the held roster, for a focus or life-cycle change, and ask
    /// a held permission prompt now that the person may be back.
    ///
    /// **An ask on the shown seat is silent while it is looked at**, so the
    /// moment the person looks away is a moment that must be read - and no
    /// frame arrives for it.
    pub fn review_asks(&self, looking: bool) {
        self.attention.store(looking, Ordering::Relaxed);
        let notices = self.asks_moved();
        self.raise(notices);
        if let Some(app) = self.app.as_ref() {
            crate::notify::review(app);
        }
    }

    /// One ask's own frame: read the roster again while the home is held.
    ///
    /// **Only a read moves the roster's pending field**, and the page's own
    /// read is not a guarantee: its heartbeat is a page timer, and WebKit
    /// stops it the instant the display sleeps. A beat newer than `FRESH_MS`
    /// therefore says nothing about whether the page will read, so the
    /// roster is always asked for here - one extra encode per ask is the
    /// cost of not missing the ask that lands in that window. A view that is
    /// not holding the home (a connect screen) asks nothing.
    fn ask_arrived(&self, update: &Value) {
        let ask = [
            "permission_request",
            "question_request",
            "browser_hand_off_pending",
            "document_hand_off_pending",
        ]
        .iter()
        .any(|key| update.get(*key).is_some());
        if !ask {
            return;
        }
        if self.lock().held.contains_key("home") {
            // A roster refresh states no scope: the held declaration decides.
            self.refresh(&json!("home"), None);
        }
    }

    /// What the page reads at the start and on every `Connection` notice.
    pub fn state(&self) -> Value {
        let state = self.lock();
        json!({
            "status": state.status,
            "generation": state.generation,
            "greeting": state.greeting,
        })
    }

    /// Send a batched settings save. `id` is the page's own number for it,
    /// echoed by the acknowledgement or the refusal. False means no socket is
    /// open and nothing went.
    pub fn settings_edit(&self, fingerprint: String, edits: Value, id: u64) -> bool {
        let state = self.lock();
        if state.status != "open" {
            return false;
        }
        if let Some(socket) = &state.socket {
            socket.settings_edit(fingerprint, edits, id);
        }
        true
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

    /// One frame of the desktop's own capture, on its way to the page.
    ///
    /// The frames go the other way from [`Bridge::frame`]: the microphone in
    /// this half produces them, and the page streams them down the take's
    /// socket - one take path for both capture legs.
    pub fn mic_frame(&self, bytes: Vec<u8>) {
        self.emit(ClientEvent::Mic(bytes));
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

    /// One repair ask: the frames a seat's stream emitted between two numbers.
    /// False means no socket is open and no answer is coming; the answer lands
    /// on the page's own `onMessage`, and costs nothing if it never comes.
    pub fn seat_range(&self, slot: &Value, epoch: Option<String>, from: u64, to: u64) -> bool {
        let state = self.lock();
        if state.status != "open" {
            return false;
        }
        if let Some(socket) = &state.socket {
            socket.seat_range(slot.clone(), epoch, from, to);
        }
        true
    }

    /// One commit's own diff. False means no socket is open and no answer is
    /// coming; the answer lands on the page's own `onMessage`.
    pub fn commit_diff(&self, session: &Value, sha: &str) -> bool {
        let state = self.lock();
        if state.status != "open" {
            return false;
        }
        if let Some(socket) = &state.socket {
            socket.commit_diff(session.clone(), sha.to_owned());
        }
        true
    }

    /// One file's whole content at a revision, for the review page's
    /// between-hunk expansion. False means no socket is open and no answer
    /// is coming; the answer lands on the page's own `onMessage`.
    pub fn file_content(&self, session: &Value, path: String, sha: Option<String>) -> bool {
        let state = self.lock();
        if state.status != "open" {
            return false;
        }
        if let Some(socket) = &state.socket {
            socket.file_content(session.clone(), path, sha);
        }
        true
    }

    /// One document read for a viewer's route. False means no socket is
    /// open and no answer is coming; the answer lands in the page's own
    /// frame stream, where the route reads it.
    pub fn read_document(&self, session: &Value, path: String, reviewable: bool) -> bool {
        let state = self.lock();
        if state.status != "open" {
            return false;
        }
        if let Some(socket) = &state.socket {
            socket.read_document(session.clone(), path, reviewable);
        }
        true
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
        // **The drain and its emits share one guard**, so a live frame
        // cannot slip between them and be overtaken by the held ones behind
        // it. `emit` is a non-blocking send, so holding the lock across it
        // costs nothing.
        let mut state = self.lock();
        while let Some(message) = state.parked.pop_front() {
            self.emit(ClientEvent::Inbound(message));
        }
        for (key, watch) in &state.held {
            let Some(session) = watch.what.get("session") else {
                // **A subject the page just re-read is not read again.** The
                // page's return and this stale beat name one return, and the
                // first beat after a lock is always stale.
                let fresh = state
                    .refreshed
                    .get(key)
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(FRESH_MS));
                if !fresh && let Some(socket) = &state.socket {
                    records::refresh(socket, &watch.what, watch.answering, watch.review);
                }
                continue;
            };
            let Some(Ok(record)) = state.records.read(key) else { continue };
            let snapshot = json!({
                "kind": "snapshot",
                "subject": watch.what,
                "data": record,
            });
            // The catch-up snapshot's frame facts (#2513): the page's byte
            // counters read every snapshot; nothing re-parses this one, so its
            // parse is zero.
            let bytes = serde_json::to_vec(&snapshot).map_or(0, |raw| raw.len());
            self.emit(ClientEvent::Inbound(snapshot));
            self.emit(ClientEvent::Framed { subject: watch.what.clone(), bytes, parse_ms: 0.0 });
            let Some(turns) = record.get("conversation").and_then(|c| c.get("turns")) else {
                continue;
            };
            // **The record's own cursor rides the page** (#2420): the
            // record's conversation is the newest window `more` with no
            // `before` would have carried, handle and all - including the
            // window's floor - so the catch-up preserves the walk above it.
            let cursor = record
                .get("conversation")
                .and_then(|conversation| conversation.get("cursor"))
                .cloned()
                .unwrap_or_default();
            // **The record's cron pairing rides the page too.** The record
            // stores the server's data whole, and this synthesized page is
            // the only read a parked desktop page gets: dropping the pairing
            // here would untitle a cron row that fired during the park
            // (#1815).
            let cron_names = record
                .get("conversation")
                .and_then(|conversation| conversation.get("cron_names"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            self.emit(ClientEvent::Inbound(json!({
                "kind": "page",
                "conversation": session,
                "cursor": cursor,
                "turns": turns,
                "cron_names": cron_names,
                // **Told apart from an answer**: a page read while an older
                // ask is "in flight" would be spliced as an older page and
                // scramble the order, so the page is marked as the catch-up
                // it is.
                "reconciled": true,
            })));
        }
    }

    fn socket_event(self: &Arc<Self>, socket_id: u64, event: SocketEvent) {
        // **A replaced socket's late events are not this connection's.** A
        // `Status::Open` from a watcher still retrying behind a fresh socket
        // would otherwise bump the generation the page reads and synthesize a
        // reconnect pair on a live wire.
        if self.lock().socket_id != socket_id {
            return;
        }
        match event {
            SocketEvent::Status(status) => self.status_moved(status),
            SocketEvent::Message { value, bytes, parse_ms } => {
                self.message(value, bytes, parse_ms);
            }
            SocketEvent::Written { key, declared, waited_ms } => {
                self.emit(ClientEvent::Written { key, declared, waited_ms });
            }
            SocketEvent::Refused { key, why } => {
                {
                    let mut state = self.lock();
                    state.records.refuse(&key, &why);
                    // The refusal IS the answer that read was asked for, so
                    // the coalescing mark comes off with it: left on, a
                    // later REPLACES for the same seat would be skipped as
                    // "already being read" forever.
                    state.refreshing.remove(&key);
                }
                self.emit(ClientEvent::Refused { key, why });
            }
            SocketEvent::Malformed(text) => {
                tauri_plugin_log::log::warn!("client socket: {text}");
            }
        }
    }

    fn status_moved(self: &Arc<Self>, status: Status) {
        {
            let mut state = self.lock();
            state.status = match status {
                Status::Connecting => "connecting",
                Status::Open => "open",
                Status::Closed => "closed",
            };
            if status == Status::Open {
                // **A socket opened**, which is the generation the page
                // compares: see the field's own comment for why a greeting
                // cannot stand in for it.
                state.generation = state.generation.saturating_add(1);
                // A fresh connection's frames are forwarded at once: the page
                // is there (it just asked to connect), and the first
                // heartbeat carries the proof rather than the promise. The
                // held frames go first - a parked page is still parked
                // across a reconnect, and they are its own.
                while let Some(message) = state.parked.pop_front() {
                    self.emit(ClientEvent::Inbound(message));
                }
                state.last_beat = Some(Instant::now());
                state.suppressed = 0;
                state.refreshing.clear();
            }
            // A drop keeps the records: the reconnect's snapshots replace
            // them whole, and nothing else on this side has to move (a take
            // is the page's own and its own status arm clears it, #1880).
        }
        self.emit(ClientEvent::Connection);
        if status == Status::Open
            && let Some((attach, generation)) = {
                let state = self.lock();
                state.attach.clone().map(|attach| (attach, state.attach_generation))
            }
        {
            let bridge = Arc::clone(self);
            tokio::spawn(async move { bridge.reattach(attach, generation).await });
        }
    }

    /// Ask a fresh connection to put the takeover's viewer back.
    ///
    /// **A viewer dies with the socket it attached on.** Without this the
    /// surface stays live over the page's last frame while every gesture goes
    /// nowhere - a picture that reads as alive over an input path that is not.
    /// A re-attach the server refuses fails the surface instead.
    async fn reattach(self: Arc<Self>, attach: Attach, generation: u64) {
        let Some(socket) = self.socket() else { return };
        match socket.browser_view(attach.seat, attach.tab).await {
            Ok(_) => {
                // A lower (or a second attach) landed while this was in
                // flight: nobody is showing the viewer this ask created, so
                // it is given back rather than left holding the profile warm.
                let stale = self.lock().attach_generation != generation;
                if stale {
                    socket.browser_detach();
                }
            }
            Err(why) => {
                let current = {
                    let mut state = self.lock();
                    let current = state.attach_generation == generation;
                    if current {
                        state.attach = None;
                        state.attach_generation = state.attach_generation.wrapping_add(1);
                    }
                    current
                };
                if current {
                    self.message(
                        json!({ "kind": "browser_surface_failed", "reason": why }),
                        0,
                        0.0,
                    );
                }
            }
        }
    }

    /// Whether a browser message is for the tab this connection views. **A
    /// chooser or a save belongs to its tab**: a viewer watching another tab
    /// neither picks nor saves for it. A message with no tab, or a connection
    /// with no attach, is not this check's to refuse.
    fn for_viewed_tab(&self, message: &Value) -> bool {
        let tab =
            message.get("tab").and_then(Value::as_u64).and_then(|tab| u16::try_from(tab).ok());
        let viewed = self.lock().attach.as_ref().and_then(|attach| attach.tab);
        match (tab, viewed) {
            (Some(tab), Some(viewed)) => tab == viewed,
            _ => true,
        }
    }

    fn message(self: &Arc<Self>, message: Value, bytes: usize, parse_ms: f64) {
        let kind = message.get("kind").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "greeting" => {
                self.lock().greeting = Some(message.clone());
                self.emit(ClientEvent::Connection);
                self.forward(&message, None);
            }
            "snapshot" => {
                let Some(subject) = message.get("subject") else { return };
                let data = message.get("data").cloned().unwrap_or(Value::Null);
                let key = records::subject_key(subject);
                let mut state = self.lock();
                state.refreshing.remove(&key);
                state.records.snapshot(subject, data);
                drop(state);
                // **A forwarded snapshot's own frame facts (#2513).** The
                // page cannot see the raw length - this half parsed it - so
                // the size and the parse travel beside the frame.
                self.forward(&message, Some((bytes, parse_ms)));
                // **The home roster is where an ask is announced.** A page
                // parked with the display off runs no timer of its own, and
                // this half has the frame either way.
                if key == "home" {
                    let notices = self.asks_moved();
                    self.raise(notices);
                }
            }
            "update" => {
                let Some(update) = message.get("update") else { return };
                let refresh = self.lock().records.apply(update);
                self.forward(&message, None);
                self.acted(refresh);
                // **An ask's own frame is the door to a read**, whether or
                // not the page is awake to ask for one: the roster's pending
                // field only moves when the home is read, and a parked page
                // never reads.
                self.ask_arrived(update);
            }
            // Consumed here alone, provably unusable by the page: a reply is
            // `dispatch`'s own answer, correlated by id in `socket.rs`.
            "reply" => {}
            // **The viewed tab's wire ids, which the overlay filters frames
            // by**: the shell takes them directly, and the page still sees
            // the frame as itself. Its controller and this connection's own
            // viewer number go to the page's bar as an event.
            "browser_view" => {
                if let Some(app) = &self.app {
                    if let (Some(id), Some(tab)) = (
                        message.get("id").and_then(Value::as_u64),
                        message.get("tab").and_then(Value::as_u64),
                    ) && let (Ok(id), Ok(tab)) = (u16::try_from(id), u16::try_from(tab))
                    {
                        crate::browser::attached(app, id, tab);
                    }
                    let _ = app.emit("browser_view", message.clone());
                }
                if let Some(profile) = message.get("profile").and_then(Value::as_str) {
                    self.lock().view_profile = Some(profile.to_owned());
                }
                self.forward(&message, None);
            }
            // **The strip follows the pushed tab list**: the page listens for
            // this as a shell event, and every change the connection asked
            // for arrives here rather than by polling.
            "browser_tabs" => {
                if let Some(app) = &self.app {
                    let _ = app.emit("browser_tabs", message.clone());
                }
                self.forward(&message, None);
            }
            // The page's surface store listens for these as shell events:
            // the history is the browser's, not something a frame reader
            // folds, and the bar draws it under the native layer.
            "browser_nav_state" => {
                if let Some(app) = &self.app {
                    let _ = app.emit(
                        "browser_nav_state",
                        json!({
                            "can_back": message.get("can_back").and_then(Value::as_bool).unwrap_or(false),
                            "can_forward": message.get("can_forward").and_then(Value::as_bool).unwrap_or(false),
                        }),
                    );
                }
                self.forward(&message, None);
            }
            // **A finished download is the shell's save door.** The bytes
            // come from the server's temp file; the person chooses where
            // they land here, and a refusal is said rather than saved.
            "browser_download" => {
                // **A save is the tab's.** The server already filters by the
                // attachment; this is the shell's own guard on a stale or
                // retargeted connection.
                if !self.for_viewed_tab(&message) {
                    self.forward(&message, None);
                    return;
                }
                let name =
                    message.get("name").and_then(Value::as_str).unwrap_or("download").to_owned();
                let data = message.get("data").and_then(Value::as_str).map(str::to_owned);
                let error = message.get("error").and_then(Value::as_str).map(str::to_owned);
                let dropped = dropped_of(&message);
                let lost = lost_of(&message);
                if let Some(app) = self.app.as_ref() {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        save_download(&app, name, data, error, dropped, lost).await;
                    });
                }
                self.forward(&message, None);
            }
            // **A page's file chooser is the shell's own panel.** The host
            // is a windowless process on the server; the person is here, so
            // the picker opens on this machine and the bytes go back under
            // the dialog's id.
            "browser_file_dialog" => {
                // **A picker is the tab's.** A dialog raised on a tab this
                // viewer is not watching is never opened here.
                if !self.for_viewed_tab(&message) {
                    self.forward(&message, None);
                    return;
                }
                let id = message.get("id").and_then(Value::as_u64);
                let multiple = message.get("multiple").and_then(Value::as_bool).unwrap_or(false);
                if let (Some(app), Some(id)) = (self.app.as_ref(), id) {
                    let previous = self.live_file.swap(id, std::sync::atomic::Ordering::AcqRel);
                    if !claim_picker(&self.picking) {
                        // **The open panel answers this ask.** The host
                        // cancelled the callback the panel was opened for as
                        // soon as this one arrived, so cancelling this ask
                        // too would leave the page with no chooser at all;
                        // the panel's answer follows the newest id instead.
                        tauri_plugin_log::log::info!(
                            "a new file chooser supersedes the open picker (event_name \
                             client_browser_file_superseded, was {previous}, id {id})"
                        );
                    } else {
                        let bridge = Arc::clone(self);
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            bridge.pick_files(&app, multiple).await;
                        });
                    }
                }
                self.forward(&message, None);
            }
            // **A refused attach is a failed surface, not a dark one.** The
            // server answers an unattachable `browser_view` with an `error`,
            // and a page that ignored it would sit under its bar with no
            // picture and no reason.
            "error" => {
                // **A refusal is in this client's own log.** The shell already
                // records the gesture leaving; without this line a refused
                // one reads as sent and then nothing, and the page's console
                // is not an artifact anyone reads after the fact.
                if let Some(what) = message.get("what").and_then(Value::as_str)
                    && what.starts_with("browser_")
                {
                    // **Only a refused gesture can be a burst.** The
                    // freshness is sampled for `browser_input` alone, so a
                    // take or an attach refusal neither reads nor moves the
                    // input burst's window.
                    let fresh = what == "browser_input" && self.refusal_is_new();
                    if refusal_is_recorded(what, fresh) {
                        tauri_plugin_log::log::info!(
                            "the browser refused a request (event_name client_browser_refused, \
                             what {what}, why {})",
                            message.get("why").and_then(Value::as_str).unwrap_or("no reason given"),
                        );
                    }
                }
                if message.get("what").and_then(Value::as_str) == Some("browser_view")
                    && let Some(app) = &self.app
                {
                    let _ = app.emit(
                        "browser_surface_failed",
                        message
                            .get("why")
                            .and_then(Value::as_str)
                            .unwrap_or("the browser could not be attached"),
                    );
                    crate::browser::lost(app);
                }
                self.forward(&message, None);
            }
            // The browser stopped: the overlay comes down at once rather than
            // leaving the last frame standing over a dead page, and the
            // page's own store draws the sentence. **The frame's identity
            // travels with it**, so a late failure for a superseded attach
            // neither lowers nor fails the surface now up.
            "browser_surface_failed" => {
                if let Some(app) = &self.app {
                    let id = message
                        .get("id")
                        .and_then(Value::as_u64)
                        .and_then(|id| u16::try_from(id).ok());
                    let tab = message
                        .get("tab")
                        .and_then(Value::as_u64)
                        .and_then(|tab| u16::try_from(tab).ok());
                    let reason = message
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("the browser stopped answering");
                    let _ = app.emit(
                        "browser_surface_failed",
                        json!({ "id": id, "tab": tab, "reason": reason }),
                    );
                    crate::browser::lost_for(app, id, tab);
                }
                self.forward(&message, None);
            }
            _ => self.forward(&message, None),
        }
    }

    /// Hand the page one frame, unless it is presumed parked.
    ///
    /// A parked spell's frames are not lost wholesale: the ones the records
    /// rebuild (a seat's chat, in the record's conversation) are only
    /// counted, and every other frame is HELD in the ring for the reconcile
    /// to flush - a released seat, a dock's ending, a dictation's close and
    /// an error are frames no record carries.
    fn forward(&self, message: &Value, framed: Option<(usize, f64)>) {
        let mut state = self.lock();
        let fresh =
            state.last_beat.is_some_and(|at| at.elapsed() < Duration::from_millis(FRESH_MS));
        if !fresh {
            state.suppressed += 1;
            if !rebuilt_by_records(message) {
                // A ring this full is a park of hours with everything but a
                // seat's chat held; on the pathological overflow the OLDEST
                // goes, and the line names what went.
                if state.parked.len() >= PARKED_RING
                    && let Some(gone) = state.parked.pop_front()
                {
                    tauri_plugin_log::log::warn!(
                        "bridge_parked_ring_full ({PARKED_RING}); dropping the oldest: {}",
                        describe(&gone)
                    );
                }
                state.parked.push_back(message.clone());
            }
            return;
        }
        drop(state);
        self.emit(ClientEvent::Inbound(message.clone()));
        // **A forwarded snapshot's own frame facts (#2513).** The page cannot
        // see the raw length - this half parsed it - so the size and the
        // parse travel beside the frame it belongs to.
        if let Some((bytes, parse_ms)) = framed
            && let Some(subject) = message.get("subject")
        {
            self.emit(ClientEvent::Framed { subject: subject.clone(), bytes, parse_ms });
        }
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
            let (what, answering, review) = (watch.what.clone(), watch.answering, watch.review);
            if let Some(socket) = &state.socket {
                records::refresh(socket, &what, answering, review);
            }
        }
    }

    /// End the connection for good, as the page's `close` does: no retry
    /// follows.
    ///
    /// **Only the connection the caller opened.** A page dials the next
    /// address before it gives the connection it replaced back, so by the
    /// time that close lands the bridge holds the new socket; closing it
    /// there would leave the new page subscribed on a dead wire.
    pub fn close(&self, socket_id: u64) {
        let state = self.lock();
        if state.socket_id != socket_id {
            return;
        }
        if let Some(socket) = &state.socket {
            socket.close();
        }
    }
}

/// Whether the records rebuild this frame for a returning page: the chat's
/// own bulk, which the record's conversation carries. **One case wide on
/// purpose** - everything else, a kind neither side knows included, is held
/// across a park rather than dropped. The held set is load-bearing beyond
/// the obvious marks: a `prompt_queued` row is folded by the PAGE (the
/// record's `state.queue` is only the snapshot's copy), so a park that ate
/// it would lose the row from the pile a reader is looking at.
fn rebuilt_by_records(message: &Value) -> bool {
    message.get("kind").and_then(Value::as_str) == Some("update")
        && message
            .get("update")
            .is_some_and(|update| records::variant_name(update) == Some("chat_appended"))
}

/// Open the platform's save door for one finished download and write it.
///
/// **A dismissed save is silent**, the way a browser's is; a refusal - the
/// file too large to relay, or a write that failed - is said out loud in a
/// dialog, because a download that vanished with no line is the failure this
/// door exists to avoid.
async fn save_download(
    app: &tauri::AppHandle,
    name: String,
    data: Option<String>,
    error: Option<String>,
    dropped: bool,
    lost: Option<u64>,
) {
    use tauri_plugin_dialog::DialogExt as _;

    if let Some(error) = error {
        say_download_refused(app, &error, dropped, lost);
        return;
    }
    let Some(data) = data else { return };
    use base64::Engine as _;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(bytes) => bytes,
        Err(why) => {
            say_download_refused(
                app,
                &format!("the download's bytes did not arrive intact: {why}"),
                false,
                None,
            );
            return;
        }
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_file_name(name).save_file(move |path| {
        let _ = tx.send(path);
    });
    let Some(path) = rx.await.ok().flatten() else { return };
    use tauri_plugin_fs::FsExt as _;
    let mut options = tauri_plugin_fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let written = app.fs().open(path, options).and_then(|mut file| {
        use std::io::Write as _;
        file.write_all(&bytes)
    });
    if let Err(why) = written {
        say_download_refused(app, &format!("the file could not be saved: {why}"), false, None);
    }
}

/// Whether a download message says the offer was dropped before a viewer
/// read it.
///
/// **Absent means a save the host finished.** An older server sends no
/// field, and a value that is not a bool is not a drop, so both read the way
/// they always did.
fn dropped_of(message: &Value) -> bool {
    message.get("dropped").and_then(Value::as_bool).unwrap_or(false)
}

/// How many offers a drop cost, when the server counted them. **The title
/// says one loss or many from it**; absent on any other download, and on a
/// server that predates the field.
fn lost_of(message: &Value) -> Option<u64> {
    message.get("lost").and_then(Value::as_u64)
}

/// The event name a download's refusal is logged under.
///
/// **A drop is not a refusal**, and a triage count of refusals must not grow
/// when the backlog sends one.
fn download_event(dropped: bool) -> &'static str {
    if dropped { "client_browser_download_dropped" } else { "client_browser_download_refused" }
}

/// The title a download's refusal shows.
///
/// **A dropped offer is not a failed save.** An offer the backlog dropped
/// never reached a save door at all, so it says so; anything that did reach
/// one - a write that failed, bytes that did not arrive intact - keeps the
/// words it always had.
fn refusal_title(dropped: bool, lost: Option<u64>) -> &'static str {
    if !dropped {
        return "the download was not saved";
    }
    // **The title agrees with the body's count.** A count that did not arrive
    // is an older server, whose drop was always titled as one loss; a known
    // many is the only case that reads plural.
    match lost {
        Some(count) if count != 1 => "downloads were dropped before they were read",
        _ => "a download was dropped before it was read",
    }
}

/// Tell the person a download did not arrive, and why.
///
/// **The line is the dialog's shadow**: a person who never saw the alert can
/// still find out what happened, and the reason is the same sentence.
#[cfg(target_os = "macos")]
fn say_download_refused(app: &tauri::AppHandle, why: &str, dropped: bool, lost: Option<u64>) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication};
    use objc2_foundation::NSString;

    tauri_plugin_log::log::info!(
        "{} (event_name {}): {why}",
        refusal_title(dropped, lost),
        download_event(dropped)
    );
    let app = app.clone();
    let why = why.to_owned();
    let title = refusal_title(dropped, lost);
    // **The alert is AppKit's own, run modally.** The dialog plugin's alert
    // picked a window of its own and was never seen (measured 2026-10-24);
    // one started from the app's own event loop is the one a person sees.
    match app.clone().run_on_main_thread(move || {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let application = NSApplication::sharedApplication(mtm);
        application.activate();
        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Critical);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(&why));
        let _ = alert.addButtonWithTitle(&NSString::from_str("OK"));
        alert.runModal();
    }) {
        Ok(()) => {}
        Err(why) => tauri_plugin_log::log::warn!(
            "the download's refusal could not reach the window (event_name \
             client_browser_download_refusal_unshown): {why}"
        ),
    }
}

#[cfg(not(target_os = "macos"))]
fn say_download_refused(app: &tauri::AppHandle, why: &str, dropped: bool, lost: Option<u64>) {
    use tauri_plugin_dialog::DialogExt as _;
    tauri_plugin_log::log::info!(
        "{} (event_name {}): {why}",
        refusal_title(dropped, lost),
        download_event(dropped)
    );
    app.dialog()
        .message(why)
        .title(refusal_title(dropped, lost))
        .kind(tauri_plugin_dialog::MessageDialogKind::Error)
        .show(|_| {});
}

/// The takeover lowered: the connection gives its viewer up. **Both
/// platforms call this** - the phone is a viewer too, and a lowered phone
/// surface that stayed attached kept its profile warm.
pub(crate) fn browser_detach(app: &tauri::AppHandle) {
    if let Some(bridge) = app.try_state::<Arc<Bridge>>() {
        bridge.browser_detach();
    }
}

/// One gesture from the native overlay, sent on the connection this process
/// holds. A click with no connection is dropped: the takeover's page is the
/// server's, and nothing else can carry it.
#[cfg(target_os = "macos")]
pub(crate) fn browser_input(
    app: &tauri::AppHandle,
    event: forge_primitives::browser::BrowserInputEvent,
) {
    if let Some(bridge) = app.try_state::<Arc<Bridge>>() {
        bridge.browser_input(event);
    }
}

/// The overlay's history arrows, on the same connection.
#[cfg(target_os = "macos")]
pub(crate) fn browser_nav(app: &tauri::AppHandle, back: bool) {
    if let Some(bridge) = app.try_state::<Arc<Bridge>>() {
        bridge.browser_nav(back);
    }
}

/// What a held frame is, for a log line: its kind and, for an update, its
/// variant.
fn describe(message: &Value) -> String {
    let kind = message.get("kind").and_then(Value::as_str).unwrap_or("?");
    match message.get("update") {
        Some(update) => {
            format!("{kind} {}", records::variant_name(update).unwrap_or("?"))
        }
        None => kind.to_owned(),
    }
}

/// Take the takeover's attachment, if one is held. The answer says whether a
/// detach is the server's to hear.
fn take_attach(state: &mut State) -> Option<Attach> {
    state.attach.take()
}

/// The perf mark a gesture leaves, or `None` where it is not a step.
///
/// **A pointer move is a stream**, one per frame at a mouse's speed, so it is
/// not marked; and a key's release is part of its press rather than a second
/// gesture (#2645).
fn gesture_mark(event: &forge_primitives::browser::BrowserInputEvent) -> Option<&'static str> {
    use forge_primitives::browser::{BrowserInputEvent, MouseAction};
    match event {
        BrowserInputEvent::Mouse { action: MouseAction::Move, .. } => None,
        BrowserInputEvent::Mouse { action: MouseAction::Press, .. } => Some("mouse:press"),
        BrowserInputEvent::Mouse { action: MouseAction::Release, .. } => Some("mouse:release"),
        BrowserInputEvent::Wheel { .. } => Some("wheel"),
        BrowserInputEvent::Key { down: false, .. } => None,
        BrowserInputEvent::Key { down: true, .. } => Some("key"),
        BrowserInputEvent::Text { .. } => Some("text"),
    }
}

/// Whether one browser refusal writes a line: **only a refused input can be
/// a burst** - a surface with no role is refused one per pointer move - so
/// its first is the record, while a refused take, attach or surface is one
/// event and always is.
fn refusal_is_recorded(what: &str, fresh: bool) -> bool {
    what != "browser_input" || fresh
}

/// **A chosen file's own name, from the path or URI the picker answered.**
/// A path is its basename; a URI's last segment is the fallback Android's
/// resolver has not already named.
fn local_name(path: &tauri_plugin_fs::FilePath) -> String {
    match path {
        tauri_plugin_fs::FilePath::Path(path) => path
            .file_name()
            .map_or_else(|| "file".to_owned(), |name| name.to_string_lossy().into_owned()),
        tauri_plugin_fs::FilePath::Url(url) => url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|last| !last.is_empty())
            .map_or_else(|| "file".to_owned(), str::to_owned),
    }
}

/// Claim the one picker slot: `true` for the caller that may open a panel,
/// `false` for an ask that arrived while one is already open.
fn claim_picker(flag: &std::sync::atomic::AtomicBool) -> bool {
    !flag.swap(true, std::sync::atomic::Ordering::AcqRel)
}

/// Android's content resolver name for a picked URI, over the shell's own
/// plugin: the URI alone carries an opaque document id rather than the name.
#[cfg(target_os = "android")]
async fn android_uri_name(app: &tauri::AppHandle, uri: &str) -> Option<String> {
    use tauri::Manager as _;

    let handle = app.try_state::<crate::android::FileName<tauri::Wry>>()?;
    let name = handle
        .0
        .run_mobile_plugin_async::<String>("name", crate::android::UriArgs { uri: uri.to_owned() })
        .await
        .ok()?;
    (!name.is_empty()).then_some(name)
}

/// Whether another chosen file is past the answer's budget, decided from its
/// metadata so a huge file is never loaded to find out.
fn past_budget(files: usize, total: u64, known: Option<u64>) -> bool {
    files >= forge_primitives::browser::MAX_UPLOAD_FILES
        || known.is_some_and(|size| {
            total.saturating_add(size) > forge_primitives::browser::MAX_UPLOAD_BYTES
        })
}

/// Read at most one byte past the remaining budget, so the caller can tell a
/// file that fits from one that grew past it without reading the whole file.
fn read_within(reader: impl std::io::Read, remaining: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;

    let mut bytes = Vec::new();
    reader.take(remaining.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::tests::stub;

    fn bridge() -> (Arc<Bridge>, mpsc::UnboundedReceiver<ClientEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Bridge::new(None, tx), rx)
    }

    /// **No socket handle is a refusal, not a hang.** A dispatch with no
    /// connection to send it on answers the page rather than leaving it on a
    /// promise that can never settle (#2688).
    #[tokio::test]
    async fn a_bridge_with_no_socket_refuses_a_dispatch() {
        let (bridge, _rx) = bridge();
        let why = bridge
            .dispatch(json!({ "cancel": {} }), false)
            .await
            .expect_err("there is no socket to send on");
        assert_eq!(why, "the socket is not open");
    }

    /// **A dropped offer is not a failed save.** The alert's title says
    /// which it is: an offer the backlog dropped never reached a save door,
    /// and a write that failed keeps the words a person already knows. The
    /// AppKit call that shows the title needs a window, so this pins the
    /// decision the call makes and not the modal itself.
    #[test]
    fn a_dropped_download_does_not_claim_a_failed_save() {
        assert_eq!(
            refusal_title(true, Some(1)),
            "a download was dropped before it was read",
            "the dropped offer's own title",
        );
        assert_eq!(
            refusal_title(true, Some(3)),
            "downloads were dropped before they were read",
            "a counted loss is not titled as one loss",
        );
        assert_eq!(
            refusal_title(true, None),
            "a download was dropped before it was read",
            "an older server's drop keeps the title it always had",
        );
        assert_eq!(
            refusal_title(false, None),
            "the download was not saved",
            "and a genuine save failure keeps the words it had",
        );
    }

    /// **A gesture is marked where it leaves, and a pointer move is not a
    /// gesture**: it is one per frame at a mouse's speed, and one line per
    /// frame would drown the file the round trip is read from (#2645). A
    /// key's release is part of its press, not a second gesture.
    #[test]
    fn a_pointer_move_is_not_a_gesture_mark() {
        use forge_primitives::browser::{BrowserInputEvent, InputModifiers, MouseAction};

        let modifiers = InputModifiers::default();
        let mouse = |action| BrowserInputEvent::Mouse {
            action,
            x: 1.0,
            y: 2.0,
            button: 0,
            clicks: 1,
            modifiers,
        };
        assert_eq!(gesture_mark(&mouse(MouseAction::Press)), Some("mouse:press"));
        assert_eq!(gesture_mark(&mouse(MouseAction::Release)), Some("mouse:release"));
        assert_eq!(gesture_mark(&mouse(MouseAction::Move)), None, "a move is a stream");

        let key = |down| BrowserInputEvent::Key {
            down,
            key: "Enter".to_owned(),
            code: "Enter".to_owned(),
            windows_vk: 13,
            modifiers,
        };
        assert_eq!(gesture_mark(&key(true)), Some("key"));
        assert_eq!(gesture_mark(&key(false)), None, "a release is not a gesture of its own");

        assert_eq!(gesture_mark(&BrowserInputEvent::Text { text: "hi".to_owned() }), Some("text"),);
        assert_eq!(
            gesture_mark(&BrowserInputEvent::Wheel {
                x: 1.0,
                y: 2.0,
                delta_x: 0.0,
                delta_y: 40.0,
                modifiers,
            }),
            Some("wheel"),
        );
    }

    /// **The drop's count is the server's, under its own key.** A save carries
    /// no count, and a value that is not a number is not one.
    #[test]
    fn the_lost_count_is_read_from_the_drop_only() {
        assert_eq!(lost_of(&json!({ "lost": 3 })), Some(3), "the server counted them");
        assert_eq!(lost_of(&json!({})), None, "a save carries none");
        assert_eq!(lost_of(&json!({ "lost": "many" })), None, "a non-number is not a count");
    }

    /// **Only an explicit true marks a dropped offer.** The flag is what the
    /// shell reads off the message; an older server sends none, and a value
    /// that is not a bool is not a drop.
    #[test]
    fn only_an_explicit_true_marks_a_dropped_offer() {
        assert!(dropped_of(&json!({ "dropped": true })), "the server says so");
        assert!(!dropped_of(&json!({})), "an older server's message is a delivered save");
        assert!(!dropped_of(&json!({ "dropped": "yes" })), "a non-bool is not a drop");
    }

    /// **A drop is not logged as a refusal.** Triage that counts refusals
    /// must not grow when the backlog drops an offer.
    #[test]
    fn a_drop_has_its_own_event_name() {
        assert_eq!(download_event(true), "client_browser_download_dropped");
        assert_eq!(download_event(false), "client_browser_download_refused");
    }

    /// **A refused gesture is one line a second, not one a frame.** The
    /// server withholds the pointer moves; the shell can only rate-limit, so
    /// a burst of refusals is recorded by its first - and a suppressed call
    /// leaves the window where it was, or a steady stream would push it
    /// forward for ever and never record again.
    #[test]
    fn a_refusal_burst_is_recorded_once_a_second() {
        let (bridge, _rx) = bridge();
        let window = || *bridge.refusal_logged.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(bridge.refusal_is_new(), "the first refusal is the record");
        let kept = window().expect("the first refusal stamped the window");
        assert!(!bridge.refusal_is_new(), "the next in the same second is not");
        assert_eq!(window(), Some(kept), "and the suppressed call left the window alone");
        *bridge.refusal_logged.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Instant::now() - Duration::from_millis(1_500));
        assert!(bridge.refusal_is_new(), "a later refusal is a new record");
    }

    /// **A refused take, attach or surface is never suppressed by a refused
    /// input's burst.** Only `browser_input` has a freshness to spend; every
    /// other refusal always writes its line.
    #[test]
    fn only_a_refused_input_can_be_a_burst() {
        assert!(refusal_is_recorded("browser_input", true), "the first of a burst");
        assert!(!refusal_is_recorded("browser_input", false), "the rest of it");
        assert!(refusal_is_recorded("browser_control", false), "a refused take always records");
        assert!(refusal_is_recorded("browser_control", true));
        assert!(refusal_is_recorded("browser_view", false), "a refused attach always records");
        assert!(refusal_is_recorded("browser_file_answer", false));
    }

    /// **The refusal line is the record the artifact carries**, so it is
    /// pinned through the frame path: a refused gesture writes it, a refused
    /// take writes its own without spending the input burst's window, and an
    /// error that is not the browser's writes nothing.
    #[test]
    fn a_refused_gesture_is_a_line_and_a_refused_take_costs_no_burst() {
        let _held = crate::log_capture::capture();
        let (bridge, _rx) = bridge();
        let refusal = |what: &str| {
            crate::log_capture::lines()
                .into_iter()
                .any(|line| line.contains("client_browser_refused") && line.contains(what))
        };

        sent(
            &bridge,
            json!({ "kind": "error", "what": "browser_control", "why": "no browser attached" }),
        );
        assert!(refusal("browser_control"), "the refused take is a line");
        sent(
            &bridge,
            json!({ "kind": "error", "what": "browser_input", "why": "not the controller" }),
        );
        assert!(
            refusal("browser_input"),
            "the refused input follows it, so the take spent no burst: {:?}",
            crate::log_capture::lines(),
        );

        let before = crate::log_capture::lines().len();
        sent(&bridge, json!({ "kind": "error", "what": "subscribe", "why": "no such subject" }));
        assert_eq!(
            crate::log_capture::lines().len(),
            before,
            "an error that is not the browser's is not a refusal: {:?}",
            crate::log_capture::lines(),
        );
    }

    /// What a bridge's ask notices went to, so a test can see a trigger fire.
    #[derive(Default)]
    struct Recorded {
        notices: Mutex<Vec<(SessionSlot, crate::asks::AskKind)>>,
    }

    impl Recorded {
        fn take(&self) -> Vec<(SessionSlot, crate::asks::AskKind)> {
            self.notices.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }
    }

    impl Alerts for Recorded {
        fn raise(&self, seat: &SessionSlot, kind: crate::asks::AskKind) {
            self.notices.lock().unwrap_or_else(PoisonError::into_inner).push((seat.clone(), kind));
        }
    }

    /// A bridge that records its ask notices instead of raising them.
    fn bridge_recording() -> (Arc<Bridge>, mpsc::UnboundedReceiver<ClientEvent>, Arc<Recorded>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let recorded = Arc::new(Recorded::default());
        let bridge = Bridge::with_alerts(None, tx, Some(Arc::clone(&recorded) as Arc<dyn Alerts>));
        (bridge, rx, recorded)
    }

    /// The home subject's snapshot, carrying `data` as the roster.
    fn home_snapshot(data: Value) -> Value {
        json!({ "kind": "snapshot", "subject": "home", "data": data })
    }

    /// One seat's row carrying an ask.
    fn asking_row(label: &str, seq: u64) -> Value {
        json!({
            "slot": { "org": "Busytools", "project": "forge", "label": label },
            "pending": "question",
            "pending_depth": 1,
            "pending_seq": seq,
        })
    }

    /// Await the next event of a kind, skipping others. **Bounded**: an
    /// event that never comes fails the test in seconds rather than hanging
    /// it (and a mutant that stops an event has to DIE, not stall).
    async fn event(
        rx: &mut mpsc::UnboundedReceiver<ClientEvent>,
        want: impl Fn(&ClientEvent) -> bool,
    ) -> ClientEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = tokio::time::timeout(left, rx.recv())
                .await
                .expect("an event within the bound")
                .expect("the bridge still feeding");
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
        let bytes = serde_json::to_string(&message).map_or(0, |text| text.len());
        let id = bridge.lock().socket_id;
        bridge.socket_event(id, SocketEvent::Message { value: message, bytes, parse_ms: 0.5 });
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
        // **A forwarded snapshot carries its frame facts beside it (#2513).**
        let ClientEvent::Framed { subject, bytes, parse_ms } =
            event(&mut rx, |event| matches!(event, ClientEvent::Framed { .. })).await
        else {
            unreachable!("filtered to framed");
        };
        assert_eq!(subject, seat(), "the facts belong to the frame's subject");
        assert!(bytes > 0, "and carry its own length");
        assert_eq!(parse_ms, 0.5, "and the parse the shell took");
        sent(&bridge, json!({ "kind": "something_new", "payload": { "a": 1 } }));
        let ClientEvent::Inbound(seen) = event(&mut rx, is_inbound()).await else {
            unreachable!("filtered to inbound");
        };
        assert_eq!(seen["kind"], "something_new");
        assert_eq!(seen["payload"]["a"], json!(1));
    }

    /// **The desktop shell's own subscribe frame carries the review scope.**
    /// The page's review open reaches the server through this frame; a Bridge
    /// that dropped the flag while keeping its own bookkeeping right would
    /// leave every desktop review read ordinary.
    #[tokio::test]
    async fn the_bridge_subscribe_frame_carries_the_review_scope() {
        let mut stub = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(stub.url.clone());
        connected(&bridge).await;

        bridge.subscribe(seat(), false, true);
        let frame = stub.heard_until(|value| value["kind"] == "subscribe").await;
        assert_eq!(frame["what"], seat(), "the seat's subscribe went");
        assert_eq!(frame["review"], json!(true), "carrying the page's review scope");
    }

    /// **The review scope crosses the Bridge, not only the socket.** The
    /// page's first read, a relink and the read bound's retry all come
    /// through these calls, so a Bridge that dropped the flag would turn them
    /// into ordinary reads while every socket test stayed green.
    #[tokio::test]
    async fn the_review_scope_survives_every_bridge_ask() {
        let (bridge, _rx) = bridge();
        let key = crate::records::subject_key(&seat());
        let review = |bridge: &Arc<Bridge>| {
            bridge.lock().held.get(&key).map(|watch| watch.review).unwrap_or(false)
        };

        bridge.subscribe(seat(), true, true);
        assert!(review(&bridge), "a subscribe carries the page's scope");

        // The bound's retry says nothing and must keep the held scope.
        bridge.refresh(&seat(), None);
        assert!(review(&bridge), "a scopeless refresh keeps the held scope");

        // The page lets the seat go: the declaration crosses without an ask.
        bridge.declare(seat(), false);
        assert!(!review(&bridge), "a declaration takes it back");
        bridge.refresh(&seat(), None);
        assert!(!review(&bridge), "and the unscoped re-ask uses it");

        // A relink states its own scope, which is the one a reconnect re-asks.
        bridge.relist(seat(), true, false);
        assert!(!review(&bridge), "a relist's own scope wins");
        bridge.relist(seat(), true, true);
        assert!(review(&bridge), "and raises it again");
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
        bridge.subscribe(seat(), true, false);
        sent(
            &bridge,
            snapshot(json!({
                "header": { "context": { "percent": 0.1 } },
                "conversation": {
                    "turns": [{ "key": "t1", "messages": [] }],
                    "cursor": "7",
                    "cron_names": { "p-cron": { "cron_id": "c1", "description": "Morning summary" } },
                },
            })),
        );
        event(&mut rx, is_inbound()).await;
        // The snapshot's frame facts ride beside it; drain them so the
        // park's own silence is the thing the assertion reads.
        let _ = event(&mut rx, |event| matches!(event, ClientEvent::Framed { .. })).await;
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
        // **The catch-up carries its frame facts too (#2513).** Without them
        // the page's byte counters miss the read and its last-snapshot time
        // stays behind.
        let ClientEvent::Framed { subject, bytes, parse_ms } =
            event(&mut rx, |event| matches!(event, ClientEvent::Framed { .. })).await
        else {
            unreachable!("filtered to framed");
        };
        assert_eq!(subject, seat(), "the catch-up's facts belong to its subject");
        assert!(bytes > 0, "and carry its own length");
        assert_eq!(parse_ms, 0.0, "nothing re-parses the record's copy");
        let ClientEvent::Inbound(page) = event(&mut rx, is_inbound_kind("page")).await else {
            unreachable!("filtered to inbound");
        };
        assert_eq!(page["cursor"], json!("7"), "the record's own handle rides the catch-up");
        assert_eq!(page["reconciled"], json!(true), "and it says which kind of page it is");
        assert_eq!(page["turns"][0]["key"], json!("t1"));
        assert_eq!(page["conversation"], seat()["session"]);
        assert_eq!(
            page["cron_names"],
            json!({ "p-cron": { "cron_id": "c1", "description": "Morning summary" } }),
            "the record's cron pairing rides the catch-up page, or a parked cron row reloads untitled",
        );
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
        bridge.subscribe(seat(), true, false);
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
        bridge.subscribe(seat(), true, false);
        bridge.subscribe(seat(), true, false);
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
        let id = bridge.lock().socket_id;
        bridge.socket_event(
            id,
            SocketEvent::Refused { key: key.to_owned(), why: "no session there".to_owned() },
        );
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

    /// **A stale heartbeat reconciles even with NOTHING counted.** A spell
    /// whose frames the webview swallowed leaves no count here, and the
    /// records still have to come back - the count is a bound on the
    /// backlog, not the trigger.
    #[tokio::test]
    async fn a_stale_heartbeat_reconciles_with_nothing_counted() {
        let (bridge, mut rx) = bridge();
        let server = stub().await;
        bridge.connect(server.url.clone());
        bridge.subscribe(seat(), true, false);
        sent(&bridge, snapshot(json!({ "conversation": { "turns": [] } })));
        event(&mut rx, is_inbound()).await;
        park(&bridge);
        bridge.heartbeat();
        assert_eq!(bridge.lock().suppressed, 0, "no frame arrived in the spell");
        let ClientEvent::Inbound(caught) = event(&mut rx, is_inbound_kind("snapshot")).await else {
            unreachable!("filtered to inbound");
        };
        assert_eq!(caught["subject"], seat(), "the record comes back regardless");
    }

    /// **A refresh re-declares what was HELD, not a capability of its own.**
    /// `(true, true)` here escalated a seat watched without flags, and the
    /// core parks turns on whatever answered.
    #[tokio::test]
    async fn a_refresh_re_declares_the_held_declaration() {
        let mut server = stub().await;
        let (bridge, mut rx) = bridge();
        bridge.connect(server.url.clone());
        bridge.subscribe(seat(), false, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        sent(&bridge, snapshot(json!({ "a": 1 })));
        event(&mut rx, is_inbound()).await;
        // A REPLACES frame: the seat is asked for again.
        sent(
            &bridge,
            json!({
                "kind": "update",
                "update": { "session_replaced": { "key": seat()["session"] } },
            }),
        );
        let _ = server.heard_until(|v| v["kind"] == "unsubscribe").await;
        let again = server.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(again["what"], seat(), "the same subject comes back");
        assert_eq!(again["answering"], false, "with the declaration it was held under");
    }

    /// **An ask's frame asks for the roster again even when the page looks
    /// awake.** The heartbeat is a page timer and stops the instant the
    /// display sleeps, so a beat inside `FRESH_MS` is not proof the page
    /// will read; the ask that lands in that window would raise no banner.
    /// An ordinary turn frame asks nothing.
    #[tokio::test]
    async fn an_asks_frame_reads_the_home_whatever_the_beat_says() {
        let mut server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), true, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        // A beat this instant is what the page's timer wrote before the
        // display slept - and the page is already frozen behind it.
        bridge.heartbeat();

        // The control: a turn frame is not an ask, so nothing is re-read.
        sent(
            &bridge,
            json!({
                "kind": "update",
                "update": { "turn_complete": { "key": seat()["session"], "terminal_reason": null } },
            }),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), server.heard.recv()).await.is_err(),
            "an ordinary frame asks for no read",
        );

        sent(
            &bridge,
            json!({
                "kind": "update",
                "update": {
                    "permission_request": {
                        "key": seat()["session"],
                        "tool_id": "tu-1",
                        "request": {},
                    }
                },
            }),
        );
        let _ = server.heard_until(|v| v["kind"] == "unsubscribe").await;
        let again = server.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(again["what"], json!("home"), "the ask reads the roster inside the beat");
        assert_eq!(again["answering"], true, "with the declaration the home is held under");
    }

    /// **A screen holding no home reads nothing for an ask.** The connect
    /// screen has no roster behind it, so an ask frame there must put no
    /// read on the wire - dropping the held-home guard would subscribe a
    /// subject the view never asked for.
    #[tokio::test]
    async fn an_asks_frame_on_a_screen_holding_no_home_reads_nothing() {
        let mut server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        connected(&bridge).await;

        sent(
            &bridge,
            json!({
                "kind": "update",
                "update": {
                    "permission_request": {
                        "key": seat()["session"],
                        "tool_id": "tu-1",
                        "request": {},
                    }
                },
            }),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), server.heard.recv()).await.is_err(),
            "a screen holding no home asks for no read",
        );
    }

    /// **A home read raises the ask it carries.** Deleting the home arm's
    /// raise would leave the roster current and the banner unsent.
    #[tokio::test]
    async fn a_home_read_raises_a_new_ask() {
        let (bridge, _rx, recorded) = bridge_recording();
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), true, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;

        // The launch's own read seeds and announces nothing.
        sent(&bridge, home_snapshot(json!({ "boot_id": "run-1", "agents": [] })));
        assert!(recorded.take().is_empty(), "launch is not news");

        sent(
            &bridge,
            home_snapshot(json!({ "boot_id": "run-1", "agents": [asking_row("w1", 1)] })),
        );
        assert_eq!(
            recorded.take(),
            vec![(SessionSlot::worker("Busytools", "forge", "w1"), crate::asks::AskKind::Question)],
            "the new ask is the one raised",
        );
    }

    /// **Clearing the shown seat is what announces the ask it held.** The ask
    /// arrived while the seat was on screen and looked at, so the home read
    /// left it unannounced; the showing that takes it off screen raises it.
    #[tokio::test]
    async fn looking_away_from_the_shown_seat_raises_its_ask() {
        let (bridge, _rx, recorded) = bridge_recording();
        bridge.review_asks(true);
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), true, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        bridge.showing(Some(json!({ "org": "Busytools", "project": "forge", "label": "lead" })));

        sent(&bridge, home_snapshot(json!({ "boot_id": "run-1", "agents": [] })));
        sent(
            &bridge,
            home_snapshot(json!({ "boot_id": "run-1", "agents": [asking_row("lead", 1)] })),
        );
        assert!(recorded.take().is_empty(), "looked at, not announced");

        bridge.showing(None);
        assert_eq!(
            recorded.take(),
            vec![(SessionSlot::lead("Busytools", "forge"), crate::asks::AskKind::Question)],
            "the seat left the screen, so its ask is news",
        );
    }

    /// **A focus event re-evaluates the roster.** The ask on the shown seat
    /// arrived while it was looked at; the event that says the person looked
    /// away is the one that raises it.
    #[tokio::test]
    async fn a_focus_change_raises_the_shown_seats_ask() {
        let (bridge, _rx, recorded) = bridge_recording();
        bridge.review_asks(true);
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), true, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        bridge.showing(Some(json!({ "org": "Busytools", "project": "forge", "label": "lead" })));

        sent(&bridge, home_snapshot(json!({ "boot_id": "run-1", "agents": [] })));
        sent(
            &bridge,
            home_snapshot(json!({ "boot_id": "run-1", "agents": [asking_row("lead", 1)] })),
        );
        assert!(recorded.take().is_empty(), "looked at, not announced");

        bridge.review_asks(false);
        assert_eq!(
            recorded.take(),
            vec![(SessionSlot::lead("Busytools", "forge"), crate::asks::AskKind::Question)],
            "the focus event is what raises the glance",
        );
    }

    /// **A fresh bridge treats the person as looking.** The window is what
    /// activated the app, so the seed is `true` and no event has to arrive
    /// before the first judged roster read. Seeding `false` would announce
    /// every ask on the shown seat as if the person had looked away.
    #[tokio::test]
    async fn a_fresh_bridge_treats_the_person_as_looking() {
        let (bridge, _rx, recorded) = bridge_recording();
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), true, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        bridge.showing(Some(json!({ "org": "Busytools", "project": "forge", "label": "lead" })));

        sent(&bridge, home_snapshot(json!({ "boot_id": "run-1", "agents": [] })));
        sent(
            &bridge,
            home_snapshot(json!({ "boot_id": "run-1", "agents": [asking_row("lead", 1)] })),
        );
        assert!(
            recorded.take().is_empty(),
            "no review_asks reached this bridge, so the seed alone keeps the looked-at seat silent",
        );
    }

    /// A subject two page readers hold is re-asked once per OPEN one on a
    /// reconnect from the connect screen: the count is the server's, and one
    /// ask would leave the subject half-watched. **A seat is not among them**
    /// - it is the page's own to re-open after a drop - so the count is
    /// pinned on a subject the connection does carry across.
    #[tokio::test]
    async fn a_subject_two_readers_hold_is_re_asked_per_reader() {
        let mut server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), true, false);
        bridge.subscribe(json!("home"), true, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        bridge.connect(server.url.clone());
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        let second = server.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(second["what"], json!("home"), "the second ask is the same subject");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), server.heard.recv()).await.is_err(),
            "exactly two: one per open subscription"
        );
    }

    /// **A replaced socket's late events are not the live connection's.** The
    /// bridge closes the old socket at the swap, but its task can still
    /// deliver a queued `Status::Open`; counting it would bump the generation
    /// the page reads and synthesize a reconnect pair on a live wire - which
    /// zeroes every visited seat's subscription count and clears a running
    /// take. The socket's own id is what tells the two apart.
    #[tokio::test]
    async fn a_replaced_sockets_late_open_does_not_touch_the_live_connection() {
        let server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        let first = bridge.lock().socket_id;
        connected(&bridge).await;

        bridge.connect(server.url.clone());
        connected(&bridge).await;
        let second = bridge.lock().socket_id;
        assert_ne!(first, second, "precondition: the bridge swapped sockets");
        let generation = bridge.lock().generation;

        bridge.socket_event(first, SocketEvent::Status(Status::Open));
        bridge.socket_event(first, SocketEvent::Status(Status::Connecting));
        let state = bridge.lock();
        assert_eq!(state.generation, generation, "a replaced socket's open bumped the generation");
        assert_eq!(state.status, "open", "a replaced socket's drop moved the live connection");
    }

    /// **A page connection that has been replaced cannot close the one that
    /// replaced it.**
    ///
    /// The page dials the next address before it gives the old connection
    /// back: `Shell.take` closes the connection it replaced only after the
    /// new attempt has greeted, and the greeting is proof the new socket is
    /// the bridge's current one. So the old `client_close` lands on a
    /// connection that is not its own - and closing it there leaves the new
    /// page subscribed on a dead wire, its fleet read never answered. That is
    /// the stall an address change leaves behind until the app restarts.
    #[tokio::test]
    async fn a_replaced_page_connections_close_leaves_the_new_socket_alone() {
        let mut first = stub().await;
        let (bridge, mut rx) = bridge();
        let old = bridge.connect(first.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), true, false);
        let _ = first.heard_until(|v| v["kind"] == "subscribe").await;
        first
            .say
            .send(json!({
                "kind": "snapshot",
                "subject": "home",
                "data": { "fleet": ["alpha"] },
            }))
            .expect("the first stub still feeding");
        let ClientEvent::Inbound(snapshot) = event(&mut rx, is_inbound_kind("snapshot")).await
        else {
            unreachable!("filtered to snapshot");
        };
        assert_eq!(snapshot["data"]["fleet"], json!(["alpha"]), "precondition: A's fleet drew");

        // **The address change, in the page's own order.** The new attempt
        // connects and greets first; only then does the shell close the
        // connection it replaced, and only then does the new page subscribe
        // the home it draws.
        let mut second = stub().await;
        bridge.connect(second.url.clone());
        connected(&bridge).await;
        bridge.close(old);
        bridge.subscribe(json!("home"), true, false);

        let mut subscribed = 0;
        let read_by = tokio::time::Instant::now() + Duration::from_secs(2);
        while subscribed < 2 {
            let left = read_by.saturating_duration_since(tokio::time::Instant::now());
            let Ok(heard) = tokio::time::timeout(left, second.heard.recv()).await else {
                panic!(
                    "the new socket never heard the fresh page's subscribe (the old page's close killed it)"
                );
            };
            let Some(heard) = heard else { break };
            if heard["kind"] == json!("subscribe") && heard["what"] == json!("home") {
                subscribed += 1;
            }
        }
        assert_eq!(subscribed, 2, "the new page's subscribe never reached the new server");
        assert_eq!(
            bridge.lock().status,
            "open",
            "the replaced connection's close shut the live socket"
        );
    }

    /// **The close that does name the live connection still ends it.** The
    /// id guards against a replaced connection's close; it is not a way to
    /// ignore one. A shell that goes takes its socket with it.
    #[tokio::test]
    async fn a_close_naming_the_live_connection_ends_it() {
        let server = stub().await;
        let (bridge, _rx) = bridge();
        let live = bridge.connect(server.url.clone());
        connected(&bridge).await;

        bridge.close(live);
        let by = tokio::time::Instant::now() + Duration::from_secs(5);
        while bridge.lock().status != "closed" {
            assert!(
                tokio::time::Instant::now() < by,
                "the live connection's own close left it open",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// **A page-driven reconnect re-attaches no seat by itself.** Only the
    /// shown seat comes back live, through the page's own `relist`; every
    /// other visited seat waits for its next click, so a reconnect does not
    /// pay a whole record each for seats nobody is looking at.
    #[tokio::test]
    async fn a_reconnect_from_the_connect_screen_asks_for_no_seat() {
        let mut server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(seat(), true, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;

        bridge.connect(server.url.clone());
        // **Wait for the new socket to be up**, or the absence proves nothing:
        // an assertion 200 ms after a dial that never landed reads the same as
        // a resume that stayed quiet.
        connected(&bridge).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), server.heard.recv()).await.is_err(),
            "a seat re-attached itself on resume instead of waiting for a click",
        );
    }

    /// **The showing crosses as the frame the server reads**, `null`
    /// included: it is a state assertion, and "nothing of mine is on screen"
    /// is one of its states.
    #[tokio::test]
    async fn a_showing_crosses_as_its_own_frame() {
        let mut server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        connected(&bridge).await;

        bridge.showing(Some(seat()["session"].clone()));
        let shown = server.heard_until(|v| v["kind"] == "showing").await;
        assert_eq!(shown["seat"], seat()["session"], "the seat rode the frame");

        bridge.showing(None);
        let cleared = server.heard_until(|v| v["kind"] == "showing").await;
        assert_eq!(cleared["seat"], Value::Null, "nothing on screen crosses as null");
    }

    /// **A showing is answered on the thread that dispatched it**, so two
    /// rapid invokes stay in order and the last seat is the one recorded. An
    /// `(async)` command hands its body to the runtime instead, and the
    /// answer arrives from another thread after the dispatch has returned.
    #[test]
    fn a_showing_is_answered_on_the_dispatching_thread() {
        use tauri::test::{MockRuntime, mock_builder, mock_context, noop_assets};

        let (tx, _rx) = mpsc::unbounded_channel();
        let bridge = Bridge::new(None, tx);
        let app = mock_builder()
            .manage(Arc::clone(&bridge))
            .invoke_handler(tauri::generate_handler![crate::commands::client_showing])
            .build(mock_context(noop_assets()))
            .expect("a mock app builds");
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("a mock window builds")
            .as_ref()
            .clone();

        let dispatch = std::thread::current().id();
        let answered = Arc::new(Mutex::new(Vec::new()));
        let send = |seat: Value| {
            let answered = Arc::clone(&answered);
            let responder: Box<tauri::ipc::OwnedInvokeResponder<MockRuntime>> =
                Box::new(move |_webview, _cmd, _response, _callback, _error| {
                    answered
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(std::thread::current().id());
                });
            webview.clone().on_message(
                tauri::webview::InvokeRequest {
                    cmd: "client_showing".into(),
                    callback: tauri::ipc::CallbackFn(0),
                    error: tauri::ipc::CallbackFn(1),
                    url: "tauri://localhost".parse().expect("a local url"),
                    body: tauri::ipc::InvokeBody::Json(json!({ "seat": seat })),
                    headers: Default::default(),
                    invoke_key: tauri::test::INVOKE_KEY.to_string(),
                },
                responder,
            );
        };

        send(seat()["session"].clone());
        assert_eq!(
            bridge.lock().shown,
            Some(SessionSlot::lead("Busytools", "forge")),
            "the first showing landed before the second was dispatched",
        );
        send(Value::Null);

        assert_eq!(
            *answered.lock().unwrap_or_else(PoisonError::into_inner),
            vec![dispatch, dispatch],
            "each showing is answered before its dispatch returns, in arrival order",
        );
        assert_eq!(bridge.lock().shown, None, "the last showing is the seat recorded");
    }

    /// **A relist asks again without counting a reader.** The page's store
    /// survived the drop, so only the wire's half went; counting a second
    /// reader here would leave an unsubscribe one short for the life of the
    /// connection.
    #[tokio::test]
    async fn a_relist_asks_without_counting_a_reader() {
        let mut server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(seat(), true, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;

        bridge.relist(seat(), false, false);
        let again = server.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(again["what"], seat(), "the relist asked for the same subject");
        assert_eq!(again["answering"], true, "the held declaration was not raised into the relist",);

        let key = records::subject_key(&seat());
        assert_eq!(
            bridge.lock().held.get(&key).map(|watch| watch.readers),
            Some(1),
            "the relist counted a second reader",
        );
    }

    /// **A release gives back what the WIRE holds, not what the page holds.**
    /// A relist raises the socket's count without raising this side's
    /// readers, so a release that looped the readers would leave one server
    /// subscription - and its work-watch hold - standing until the
    /// connection closed.
    #[tokio::test]
    async fn a_release_gives_back_the_wire_count_after_a_relist() {
        let mut server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(seat(), false, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        // A relist asks once more on the wire while the reader count stays 1.
        bridge.relist(seat(), false, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;

        bridge.release(&seat());
        let _ = server.heard_until(|v| v["kind"] == "unsubscribe").await;
        let _ = server.heard_until(|v| v["kind"] == "unsubscribe").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), server.heard.recv()).await.is_err(),
            "the release gave back fewer entries than the wire held",
        );
    }

    /// A subject that is not a seat is re-read on the reconcile rather than
    /// handed back: the home roster's copy here serves the ask rule's own
    /// fold, and the page reads the roster itself, so a re-subscribe is what
    /// answers it with the truth.
    #[tokio::test]
    async fn a_reconcile_re_asks_a_subject_that_is_not_a_seat() {
        let mut server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), true, false);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        park(&bridge);
        bridge.heartbeat();
        let _ = server.heard_until(|v| v["kind"] == "unsubscribe").await;
        let again = server.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(again["what"], json!("home"), "the home is re-read, not copied");
    }

    /// A fresh heartbeat reconciles NOTHING: the reconcile answers a spell,
    /// it is not a per-beat copy of the records. The connection's own status
    /// events are not the subject - only an Inbound frame would be.
    #[tokio::test]
    async fn a_fresh_heartbeat_reconciles_nothing() {
        let (bridge, mut rx) = bridge();
        let server = stub().await;
        bridge.connect(server.url.clone());
        bridge.subscribe(seat(), true, false);
        sent(&bridge, snapshot(json!({ "conversation": { "turns": [] } })));
        event(&mut rx, is_inbound()).await;
        bridge.heartbeat();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        while tokio::time::Instant::now() < deadline {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Some(ClientEvent::Inbound(message))) => {
                    panic!("a record copy on a beat that found the page alive: {message}");
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
    }

    /// The ring is bounded and the OLDEST goes when it overflows - the
    /// newest are the frames a returning page would still be meeting.
    #[tokio::test]
    async fn the_ring_drops_the_oldest_when_full() {
        let (bridge, _rx) = bridge();
        let server = stub().await;
        bridge.connect(server.url.clone());
        park(&bridge);
        for at in 0..=PARKED_RING {
            sent(
                &bridge,
                json!({
                    "kind": "update",
                    "update": { "releasing": { "key": seat()["session"], "at": at } },
                }),
            );
        }
        let state = bridge.lock();
        assert_eq!(state.parked.len(), PARKED_RING, "the cap holds");
        assert_eq!(
            state.parked.front().and_then(|message| message["update"]["releasing"]["at"].as_u64()),
            Some(1),
            "the oldest went, not the newest"
        );
    }

    /// Answer one attach the stub heard, as the server's `ok` reply naming
    /// the profile and tab it attached (tab 1 when the request named none).
    async fn answer_attach(server: &mut crate::socket::tests::Stub) {
        let sent = server.heard_until(|v| v["kind"] == "browser_view").await;
        let id = sent["reply_to"].as_u64().expect("the asker minted a reply id");
        let tab = sent["tab"].as_u64().unwrap_or(1);
        server
            .say
            .send(json!({
                "kind": "reply",
                "reply_to": id,
                "body": { "ok": true, "profile": "Busytools/work", "tab": tab },
            }))
            .expect("broadcast");
    }

    /// **A live takeover's viewer dies with the socket it attached on.** The
    /// server drops a viewer with its connection, so a reconnect that said
    /// nothing left the surface drawing the page's last frame while every
    /// gesture went nowhere - a picture that reads as alive over dead input.
    /// The reconnect asks again, with the seat and the tab that landed.
    #[tokio::test]
    async fn a_live_takeover_reattaches_after_a_reconnect() {
        let (bridge, _rx) = bridge();
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        let opening = tokio::spawn({
            let bridge = Arc::clone(&bridge);
            async move { bridge.browser_view(seat(), Some(4)).await }
        });
        answer_attach(&mut server).await;
        opening.await.expect("the task joins").expect("the attach landed");

        // The connection drops, and the bridge dials again on its own.
        server.kill.send(()).expect("the stub drops its connections");
        let again = server.heard_until(|v| v["kind"] == "browser_view").await;
        assert_eq!(again["seat"], seat(), "the same seat is asked for");
        assert_eq!(again["tab"], json!(4), "and the same tab");
    }

    /// **A re-attach the server refuses fails the surface**, rather than
    /// leaving it live over the last kept frame: a takeover the server will
    /// not put a viewer on has no page behind it, and the picture must say so.
    #[tokio::test]
    async fn a_refused_reattach_fails_the_surface() {
        let (bridge, mut rx) = bridge();
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        let opening = tokio::spawn({
            let bridge = Arc::clone(&bridge);
            async move { bridge.browser_view(seat(), Some(4)).await }
        });
        answer_attach(&mut server).await;
        opening.await.expect("the task joins").expect("the attach landed");

        server.kill.send(()).expect("the stub drops its connections");
        let again = server.heard_until(|v| v["kind"] == "browser_view").await;
        let id = again["reply_to"].as_u64().expect("the asker minted a reply id");
        server
            .say
            .send(json!({
                "kind": "reply",
                "reply_to": id,
                "body": { "ok": false, "why": "the browser could not be attached" },
            }))
            .expect("broadcast");
        let caught = event(&mut rx, is_inbound_kind("browser_surface_failed")).await;
        let ClientEvent::Inbound(frame) = caught else { unreachable!("filtered to inbound") };
        assert_eq!(frame["reason"], json!("the browser could not be attached"));
        assert!(
            bridge.lock().attach.is_none(),
            "a failed attach is not asked for again on the next reconnect"
        );
    }

    /// **A refused Open is the surface's failure, not the last tab's
    /// picture.** The page's own Open asks directly; its refusal must lower
    /// the overlay and clear the attach, exactly as a reconnect's does.
    #[tokio::test]
    async fn a_refused_open_lowers_the_previous_surface() {
        let (bridge, mut rx) = bridge();
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        let opening = tokio::spawn({
            let bridge = Arc::clone(&bridge);
            async move { bridge.browser_view(seat(), Some(4)).await }
        });
        answer_attach(&mut server).await;
        opening.await.expect("the task joins").expect("the attach landed");

        let refused = tokio::spawn({
            let bridge = Arc::clone(&bridge);
            async move { bridge.browser_view(seat(), Some(9)).await }
        });
        let asked = server.heard_until(|v| v["kind"] == "browser_view").await;
        let id = asked["reply_to"].as_u64().expect("the asker minted a reply id");
        server
            .say
            .send(json!({
                "kind": "reply",
                "reply_to": id,
                "body": { "ok": false, "why": "tab 9 is not open" },
            }))
            .expect("broadcast");

        assert!(
            refused.await.expect("the task joins").is_err(),
            "the refusal is the caller's answer"
        );
        let caught = event(&mut rx, is_inbound_kind("browser_surface_failed")).await;
        let ClientEvent::Inbound(frame) = caught else { unreachable!("filtered to inbound") };
        assert_eq!(frame["reason"], json!("tab 9 is not open"));
        assert!(bridge.lock().attach.is_none(), "the dead tab is not asked for again");
    }

    /// **A lowered takeover stays lowered across a reconnect.** The re-attach
    /// follows the surface, not the profile: asking again after Done would put
    /// a viewer on a browser nobody is looking at, keeping the profile warm
    /// against the idle clock it just gave up.
    #[tokio::test]
    async fn a_lowered_takeover_does_not_reattach() {
        let (bridge, _rx) = bridge();
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), false, false);
        let opening = tokio::spawn({
            let bridge = Arc::clone(&bridge);
            async move { bridge.browser_view(seat(), Some(4)).await }
        });
        answer_attach(&mut server).await;
        opening.await.expect("the task joins").expect("the attach landed");
        bridge.browser_detach();
        let _ = server.heard_until(|v| v["kind"] == "browser_detach").await;

        server.kill.send(()).expect("the stub drops its connections");
        // The reconnect's re-ask proves the connection is back; every frame
        // from there through the window is inspected for a second attach.
        let mut kinds = Vec::new();
        loop {
            let heard = server.next_heard().await;
            kinds.push(heard["kind"].clone());
            if heard["kind"] == json!("subscribe") {
                break;
            }
        }
        while let Ok(Some(heard)) =
            tokio::time::timeout(Duration::from_millis(500), server.heard.recv()).await
        {
            kinds.push(heard["kind"].clone());
        }
        assert!(
            !kinds.contains(&json!("browser_view")),
            "a lowered takeover is not attached again: {kinds:?}"
        );
    }

    /// **A lower with nothing attached says nothing.** The page lowers from
    /// more than one path for one takeover; each used to tell the server to
    /// detach an attachment that was already gone.
    #[test]
    fn a_detach_sends_only_when_a_takeover_was_attached() {
        let (bridge, _rx) = bridge();
        assert!(take_attach(&mut bridge.lock()).is_none(), "nothing was attached");
        bridge.lock().attach = Some(Attach { seat: seat(), tab: None });
        assert!(take_attach(&mut bridge.lock()).is_some(), "the first lower finds it");
        assert!(take_attach(&mut bridge.lock()).is_none(), "the second finds nothing");
    }

    /// **A download or a file ask belongs to the tab on screen.** The shell
    /// forwards another tab's to the page so a strip can draw it, but opening
    /// the save dialog or the picker would take over the person's view of a
    /// page they are not looking at.
    #[test]
    fn a_message_for_another_tab_is_not_the_viewed_pages() {
        let (bridge, _rx) = bridge();
        bridge.lock().attach = Some(Attach { seat: seat(), tab: Some(2) });
        assert!(bridge.for_viewed_tab(&json!({ "tab": 2 })), "the viewed tab's message");
        assert!(!bridge.for_viewed_tab(&json!({ "tab": 3 })), "another tab's is not the page's");
        assert!(bridge.for_viewed_tab(&json!({})), "a message that names no tab is not filtered",);
        bridge.lock().attach = Some(Attach { seat: seat(), tab: None });
        assert!(
            bridge.for_viewed_tab(&json!({ "tab": 3 })),
            "before an attach names its tab, nothing is another tab's",
        );
    }

    /// **One panel at a time.** A newer ask that arrives while a picker is
    /// open does not stack a second panel; the open panel answers it.
    #[test]
    fn a_second_picker_claim_is_refused_while_one_is_open() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        assert!(claim_picker(&flag), "the first ask opens the panel");
        assert!(!claim_picker(&flag), "a second ask does not open a panel of its own");
        flag.store(false, std::sync::atomic::Ordering::Release);
        assert!(claim_picker(&flag), "and the next ask after it opens one again");
    }

    /// **The open panel answers the newest ask.** The host cancels the
    /// callback the panel was opened for as soon as a second chooser arrives,
    /// so the panel's answer must follow the live id; `pick_files` takes no
    /// id for that reason and reads this one when the panel resolves.
    #[test]
    fn an_open_pickers_answer_follows_the_newest_ask() {
        let (bridge, _rx) = bridge();
        bridge.live_file.store(4, std::sync::atomic::Ordering::Release);
        assert_eq!(bridge.live_file.load(std::sync::atomic::Ordering::Acquire), 4);
        bridge.live_file.store(9, std::sync::atomic::Ordering::Release);
        assert_eq!(
            bridge.live_file.load(std::sync::atomic::Ordering::Acquire),
            9,
            "the newest ask is the one the panel's answer belongs to"
        );
    }

    /// **The page sees the file's own name.** A path answer keeps its
    /// basename, and a URI answer falls back to its last segment until
    /// Android's resolver names the document.
    #[test]
    fn a_chosen_files_name_is_its_own() {
        use tauri_plugin_fs::FilePath;

        assert_eq!(local_name(&FilePath::Path("/tmp/resume.pdf".into())), "resume.pdf");
        assert_eq!(local_name(&FilePath::Path("/tmp/0-resume.pdf".into())), "0-resume.pdf");
        let url =
            tauri::Url::parse("content://com.android.providers/document/msf%3A42").expect("a uri");
        assert_eq!(local_name(&FilePath::Url(url)), "msf%3A42");
    }

    /// **A file whose size is past the budget is refused before a byte is
    /// read**, and the running total is checked the same way; a client that
    /// read first would pull a huge file into memory only to refuse it.
    #[test]
    fn a_file_past_the_budget_is_refused_before_it_is_read() {
        let cap = forge_primitives::browser::MAX_UPLOAD_BYTES;
        assert!(!past_budget(0, 0, Some(cap)), "exactly the cap fits");
        assert!(past_budget(0, 0, Some(cap + 1)), "one byte over is refused");
        assert!(past_budget(0, cap, Some(1)), "the running total counts");
        assert!(!past_budget(0, 0, None), "an unknown size is read within the budget instead");
        assert!(
            past_budget(forge_primitives::browser::MAX_UPLOAD_FILES, 0, Some(1)),
            "past the file count is refused whatever its size"
        );
    }

    /// **The read stops one byte past the budget.** A reader with no end is
    /// asked for `remaining + 1` and no further, which is how a file that
    /// grew past the cap is told from one that fits.
    #[test]
    fn a_read_stops_one_byte_past_the_budget() {
        struct Endless {
            given: usize,
        }
        impl std::io::Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.given >= 9 {
                    return Err(std::io::Error::other("read past the budget"));
                }
                let count = buf.len().min(9 - self.given);
                buf[..count].fill(b'x');
                self.given += count;
                Ok(count)
            }
        }

        let bytes = read_within(Endless { given: 0 }, 8)
            .expect("the endless reader is asked for one byte past the budget");
        assert_eq!(bytes.len(), 9, "one byte past the budget is the answer");
    }

    /// **The picker's answer crosses under the dialog's id**, with the file
    /// name and base64 bytes the server decodes into the upload.
    #[tokio::test]
    async fn a_file_answer_crosses_the_socket_under_its_id() {
        let (bridge, _rx) = bridge();
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;

        bridge.answer_files(4, vec![json!({ "name": "cv.pdf", "data": "aGVsbG8=" })]);

        let heard = server.heard_until(|value| value["kind"] == "browser_file_answer").await;
        assert_eq!(heard["id"], 4, "under the id the chooser was asked with");
        assert_eq!(heard["files"][0]["name"], "cv.pdf");
        assert_eq!(heard["files"][0]["data"], "aGVsbG8=", "the bytes cross base64");
    }

    /// **One return reads home once.** The page's own return re-read asks
    /// first (`connect/resync.ts`); the first heartbeat after a lock is always
    /// stale, and the reconcile must not ask for the same subject again - the
    /// home encode walks every project's working tree.
    #[tokio::test]
    async fn a_return_does_not_read_home_twice() {
        let (bridge, _rx) = bridge();
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), true, false);
        let first = server.heard_until(|frame| frame["kind"] == "subscribe").await;
        assert_eq!(first["what"], json!("home"), "the page holds home");

        // The page's return re-read, as resync sends it.
        bridge.refresh(&json!("home"), None);
        let again = server.heard_until(|frame| frame["kind"] == "subscribe").await;
        assert_eq!(again["what"], json!("home"), "the return asks for home");

        // The first beat after the park is stale, and reconciles.
        park(&bridge);
        bridge.heartbeat();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), server.heard.recv()).await.is_err(),
            "the reconcile read home again on the same return",
        );
    }
}
