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
//! rebuilds those. A subject that is not a seat is re-read instead of
//! handed a copy: nothing here folds a home record.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tauri::Emitter as _;
use tauri::Manager as _;
use tokio::sync::mpsc;

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
    profile: Option<String>,
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
    status: &'static str,
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
    /// The takeover's live viewer, re-attached on every reconnect while it
    /// is set; a lower clears it.
    attach: Option<Attach>,
    /// Which attach (or lower) is current. A re-attach that lands after a
    /// later lower or a second attach is stale, and takes itself back.
    attach_generation: u64,
}

/// The app half, as the Tauri layer drives it.
pub struct Bridge {
    state: Mutex<State>,
    /// The app, for the one sink the socket needs: relayed browser frames
    /// land in the viewer, which draws on the main thread. `None` in a test
    /// that never draws one.
    app: Option<tauri::AppHandle>,
    events: mpsc::UnboundedSender<ClientEvent>,
    /// The wire sequence for the viewer's input, minted here so the overlay
    /// and the page share one ordered stream.
    input_seq: AtomicU64,
    /// **One picker at a time.** A newer ask while a panel is open would
    /// otherwise open a second panel for a dialog the first is already
    /// answering; the open panel's answer follows the newest ask instead.
    picking: std::sync::atomic::AtomicBool,
    /// The ask the open panel's answer belongs to.
    live_file: AtomicU64,
}

/// Clears the picker flag however the picker ends, a panic included.
struct Picking<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Picking<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl Bridge {
    pub fn new(
        app: Option<tauri::AppHandle>,
        events: mpsc::UnboundedSender<ClientEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                records: Records::new(),
                held: HashMap::new(),
                order: Vec::new(),
                socket: None,
                status: "connecting",
                greeting: None,
                last_beat: None,
                suppressed: 0,
                parked: VecDeque::new(),
                refreshing: HashSet::new(),
                attach: None,
                attach_generation: 0,
            }),
            app,
            events,
            input_seq: AtomicU64::new(0),
            picking: std::sync::atomic::AtomicBool::new(false),
            live_file: AtomicU64::new(0),
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

    /// Attach this connection to a profile's browser as a viewer, answering
    /// the profile the server attached: a refused attach is the surface's
    /// sentence. **The answer is the name the surface's Done lowers**, since
    /// a raise that named none resolved to the asking seat's own.
    pub async fn browser_view(
        &self,
        seat: Value,
        profile: Option<String>,
    ) -> Result<String, String> {
        let socket = self.socket().ok_or_else(|| "the socket is not open".to_owned())?;
        let attached = socket.browser_view(seat.clone(), profile.clone()).await?;
        // **A landed attach is the takeover's to keep**: the next reconnect
        // asks for it again, so the surface never outlives its viewer. The
        // resolved name is kept, not the request's own, so a reconnect asks
        // for the very profile that came up.
        let mut state = self.lock();
        state.attach = Some(Attach { seat, profile: Some(attached.clone()) });
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
            socket.browser_input(seq, event);
        }
    }

    /// Take the controller role, or give it back.
    pub fn browser_control(&self, take: bool) {
        if let Some(socket) = self.socket() {
            socket.browser_control(take);
        }
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

    /// The profiles this server holds, for the strip's rows.
    pub async fn browser_profiles(&self, seat: Value) -> Result<Value, String> {
        let socket = self.socket().ok_or_else(|| "the socket is not open".to_owned())?;
        socket.browser_profiles(seat).await
    }

    /// Close a named profile's browser, keeping its logins.
    pub async fn browser_profile_close(&self, seat: Value, name: &str) -> Result<(), String> {
        let socket = self.socket().ok_or_else(|| "the socket is not open".to_owned())?;
        socket.browser_profile_close(seat, name).await
    }

    /// Delete a named profile and its logins.
    pub async fn browser_profile_delete(&self, seat: Value, name: &str) -> Result<(), String> {
        let socket = self.socket().ok_or_else(|| "the socket is not open".to_owned())?;
        socket.browser_profile_delete(seat, name).await
    }

    /// Dial the server, replacing any connection this bridge had. Whatever
    /// the page already watches is re-asked on the new socket, so a
    /// reconnect from the connect screen lands the subscriptions it had.
    pub fn connect(self: &Arc<Self>, url: String) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let frames: crate::socket::FrameSink = match &self.app {
            Some(app) => {
                let app = app.clone();
                Arc::new(move |bytes: &[u8]| crate::browser::frame(&app, bytes))
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
        let wanted: Vec<(Value, bool, usize)> = {
            let mut state = self.lock();
            // **The park outlives the connection it was kept across**: a
            // page away when the socket drops is still away, so the held
            // frames are handed over before the ring is emptied rather than
            // discarded for a fresh start. Emitting into a parked page's
            // queue is bounded by the ring's own cap.
            while let Some(message) = state.parked.pop_front() {
                self.emit(ClientEvent::Inbound(message));
            }
            state.socket = Some(socket.clone());
            state.status = "connecting";
            state.greeting = None;
            state.last_beat = Some(Instant::now());
            state.suppressed = 0;
            state.refreshing.clear();
            let mut wanted = Vec::with_capacity(state.order.len());
            for key in &state.order {
                if let Some(watch) = state.held.get(key) {
                    wanted.push((watch.what.clone(), watch.answering, watch.readers));
                }
            }
            wanted
        };
        for (what, answering, readers) in wanted {
            // Once per OPEN subscription: the server counts, and the new
            // socket starts at zero - a subject two page readers hold has to
            // be asked for twice, or the first unsubscribe silences it.
            for _ in 0..readers {
                socket.subscribe(what.clone(), answering);
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
    pub fn subscribe(&self, what: Value, answering: bool) {
        let key = records::subject_key(&what);
        if key.is_empty() {
            return;
        }
        let mut state = self.lock();
        match state.held.get_mut(&key) {
            Some(watch) => {
                watch.readers += 1;
                watch.answering |= answering;
            }
            None => {
                state.order.push(key.clone());
                state.held.insert(key, Watch { what: what.clone(), readers: 1, answering });
            }
        }
        if let Some(socket) = &state.socket {
            socket.subscribe(what, answering);
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

    /// Ask again for a subject the page already holds: the core answers with
    /// a snapshot, which replaces the record whole. **The declaration is the
    /// held one**: a refresh with flags of its own would escalate a seat
    /// watched without them, and the core parks turns on whatever answered.
    pub fn refresh(&self, what: &Value) {
        let state = self.lock();
        let key = records::subject_key(what);
        let answering = state.held.get(&key).map(|watch| watch.answering).unwrap_or(false);
        if let Some(socket) = &state.socket {
            records::refresh(socket, what, answering);
        }
    }

    /// What the page reads at the start and on every `Connection` notice.
    pub fn state(&self) -> Value {
        let state = self.lock();
        json!({
            "status": state.status,
            "greeting": state.greeting,
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
                if let Some(socket) = &state.socket {
                    records::refresh(socket, &watch.what, watch.answering);
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
            self.emit(ClientEvent::Inbound(json!({
                "kind": "page",
                "conversation": session,
                "cursor": cursor,
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
        match socket.browser_view(attach.seat, attach.profile).await {
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
            }
            "update" => {
                let Some(update) = message.get("update") else { return };
                let refresh = self.lock().records.apply(update);
                self.forward(&message, None);
                self.acted(refresh);
            }
            // Consumed here alone, provably unusable by the page: a reply is
            // `dispatch`'s own answer, correlated by id in `socket.rs`.
            "reply" => {}
            // **The viewed profile's wire id, which the overlay filters
            // frames by**: the shell takes it directly, and the page still
            // sees the frame as itself. Its controller and this connection's
            // own viewer number go to the page's bar as an event.
            "browser_view" => {
                if let Some(app) = &self.app {
                    if let Some(id) = message.get("id").and_then(Value::as_u64)
                        && let Ok(id) = u16::try_from(id)
                    {
                        crate::browser::attached(app, id);
                    }
                    let _ = app.emit("browser_view", message.clone());
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
                let name =
                    message.get("name").and_then(Value::as_str).unwrap_or("download").to_owned();
                let data = message.get("data").and_then(Value::as_str).map(str::to_owned);
                let error = message.get("error").and_then(Value::as_str).map(str::to_owned);
                if let Some(app) = self.app.as_ref() {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        save_download(&app, name, data, error).await;
                    });
                }
                self.forward(&message, None);
            }
            // **A page's file chooser is the shell's own panel.** The host
            // is a windowless process on the server; the person is here, so
            // the picker opens on this machine and the bytes go back under
            // the dialog's id.
            "browser_file_dialog" => {
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
            // page's own store draws the sentence.
            "browser_surface_failed" => {
                if let Some(app) = &self.app {
                    let _ = app.emit(
                        "browser_surface_failed",
                        message
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("the browser stopped answering"),
                    );
                    crate::browser::lost(app);
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
            let (what, answering) = (watch.what.clone(), watch.answering);
            if let Some(socket) = &state.socket {
                records::refresh(socket, &what, answering);
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
) {
    use tauri_plugin_dialog::DialogExt as _;

    if let Some(error) = error {
        say_not_saved(app, &error);
        return;
    }
    let Some(data) = data else { return };
    use base64::Engine as _;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(bytes) => bytes,
        Err(why) => {
            say_not_saved(app, &format!("the download's bytes did not arrive intact: {why}"));
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
        say_not_saved(app, &format!("the file could not be saved: {why}"));
    }
}

/// Tell the person a download was not saved, and why.
///
/// **The line is the dialog's shadow**: a person who never saw the alert can
/// still find out what happened, and the reason is the same sentence.
#[cfg(target_os = "macos")]
fn say_not_saved(app: &tauri::AppHandle, why: &str) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication};
    use objc2_foundation::NSString;

    tauri_plugin_log::log::info!(
        "a download was not saved (event_name client_browser_download_refused): {why}"
    );
    let app = app.clone();
    let why = why.to_owned();
    // **The alert is AppKit's own, run modally.** The dialog plugin's alert
    // picked a window of its own and was never seen (measured 2026-10-24);
    // one started from the app's own event loop is the one a person sees.
    match app.clone().run_on_main_thread(move || {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let application = NSApplication::sharedApplication(mtm);
        application.activate();
        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Critical);
        alert.setMessageText(&NSString::from_str("the download was not saved"));
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
fn say_not_saved(app: &tauri::AppHandle, why: &str) {
    use tauri_plugin_dialog::DialogExt as _;
    tauri_plugin_log::log::info!(
        "a download was not saved (event_name client_browser_download_refused): {why}"
    );
    app.dialog()
        .message(why)
        .title("the download was not saved")
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
        bridge.socket_event(SocketEvent::Message { value: message, bytes, parse_ms: 0.5 });
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

    /// A parked page's frames stop being emitted (the records still take
    /// them), and the first heartbeat after the spell answers with the
    /// catch-up: the held record as its own snapshot, and the newest page its
    /// conversation makes.
    #[tokio::test]
    async fn a_parked_spell_reconciles_from_the_records() {
        let (bridge, mut rx) = bridge();
        let server = stub().await;
        bridge.connect(server.url.clone());
        bridge.subscribe(seat(), true);
        sent(
            &bridge,
            snapshot(json!({
                "header": { "context": { "percent": 0.1 } },
                "conversation": {
                    "turns": [{ "key": "t1", "messages": [] }],
                    "cursor": "7",
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
        bridge.subscribe(seat(), true);
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
        bridge.subscribe(seat(), true);
        bridge.subscribe(seat(), true);
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

    /// **A stale heartbeat reconciles even with NOTHING counted.** A spell
    /// whose frames the webview swallowed leaves no count here, and the
    /// records still have to come back - the count is a bound on the
    /// backlog, not the trigger.
    #[tokio::test]
    async fn a_stale_heartbeat_reconciles_with_nothing_counted() {
        let (bridge, mut rx) = bridge();
        let server = stub().await;
        bridge.connect(server.url.clone());
        bridge.subscribe(seat(), true);
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
        bridge.subscribe(seat(), false);
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

    /// A subject two page readers hold is re-asked once per OPEN one on a
    /// reconnect from the connect screen: the count is the server's, and one
    /// ask would leave the seat half-watched.
    #[tokio::test]
    async fn a_subject_two_readers_hold_is_re_asked_per_reader() {
        let mut server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(seat(), true);
        bridge.subscribe(seat(), true);
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        bridge.connect(server.url.clone());
        let _ = server.heard_until(|v| v["kind"] == "subscribe").await;
        let second = server.heard_until(|v| v["kind"] == "subscribe").await;
        assert_eq!(second["what"], seat(), "the second ask is the same subject");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), server.heard.recv()).await.is_err(),
            "exactly two: one per open subscription"
        );
    }

    /// A subject that is not a seat is re-read on the reconcile rather than
    /// handed a copy: nothing on this side folds a home record, so a copy
    /// would be stale while a re-subscribe answers with the truth.
    #[tokio::test]
    async fn a_reconcile_re_asks_a_subject_that_is_not_a_seat() {
        let mut server = stub().await;
        let (bridge, _rx) = bridge();
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        bridge.subscribe(json!("home"), true);
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
        bridge.subscribe(seat(), true);
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
    /// the profile it attached (`@shared` when the request named none).
    async fn answer_attach(server: &mut crate::socket::tests::Stub) {
        let sent = server.heard_until(|v| v["kind"] == "browser_view").await;
        let id = sent["reply_to"].as_u64().expect("the asker minted a reply id");
        let profile = sent["profile"].as_str().unwrap_or("@shared").to_owned();
        server
            .say
            .send(json!({
                "kind": "reply",
                "reply_to": id,
                "body": { "ok": true, "profile": profile },
            }))
            .expect("broadcast");
    }

    /// **A live takeover's viewer dies with the socket it attached on.** The
    /// server drops a viewer with its connection, so a reconnect that said
    /// nothing left the surface drawing the page's last frame while every
    /// gesture went nowhere - a picture that reads as alive over dead input.
    /// The reconnect asks again, with the seat and profile the open named.
    #[tokio::test]
    async fn a_live_takeover_reattaches_after_a_reconnect() {
        let (bridge, _rx) = bridge();
        let mut server = stub().await;
        bridge.connect(server.url.clone());
        connected(&bridge).await;
        let opening = tokio::spawn({
            let bridge = Arc::clone(&bridge);
            async move { bridge.browser_view(seat(), Some("held".to_owned())).await }
        });
        answer_attach(&mut server).await;
        opening.await.expect("the task joins").expect("the attach landed");

        // The connection drops, and the bridge dials again on its own.
        server.kill.send(()).expect("the stub drops its connections");
        let again = server.heard_until(|v| v["kind"] == "browser_view").await;
        assert_eq!(again["seat"], seat(), "the same seat is asked for");
        assert_eq!(again["profile"], json!("held"), "and the same profile");
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
            async move { bridge.browser_view(seat(), Some("held".to_owned())).await }
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
        bridge.subscribe(json!("home"), false);
        let opening = tokio::spawn({
            let bridge = Arc::clone(&bridge);
            async move { bridge.browser_view(seat(), Some("held".to_owned())).await }
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
        bridge.lock().attach = Some(Attach { seat: seat(), profile: None });
        assert!(take_attach(&mut bridge.lock()).is_some(), "the first lower finds it");
        assert!(take_attach(&mut bridge.lock()).is_none(), "the second finds nothing");
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
}
