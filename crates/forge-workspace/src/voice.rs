//! The per-seat voice service: listeners, the sentence queue, and synthesis.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use forge_primitives::{Message, SessionSlot};
use forge_tts::{Chunk, Engine, SpeakRequest};
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;

/// What one utterance renders with, from the `[tts]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSettings {
    pub enabled: bool,
    pub max_utterance_seconds: u64,
    /// The global voice; absent uses the engine's default.
    pub voice: Option<String>,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self { enabled: true, max_utterance_seconds: 120, voice: None }
    }
}

/// The line a voice preview speaks, so two previews can be compared.
pub const PREVIEW_LINE: &str = "This is how this seat's voice sounds.";

/// Why a `say` did not reach the sink.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SayError {
    /// No client has voice on for this seat, so there is nowhere to play.
    #[error("no listener")]
    NoListener,
    /// The engine could not load or render; the chat is unaffected.
    #[error("voice engine failed: {0}")]
    Engine(String),
    /// The text is past `[tts] max_utterance_seconds`.
    #[error("{0}")]
    TooLong(String),
    /// The queue is gone, or the render stopped before its first audio.
    #[error("the voice engine is not ready")]
    NotReady,
    /// `[tts] enabled` is false, so nothing may synthesise.
    #[error("speech is off")]
    Disabled,
}

/// Where a rendered clip goes.
pub trait AudioSink: Send + Sync {
    fn push(&self, seat: &SessionSlot, chunk: Chunk);
}

/// How a listener note reaches the seat's session.
pub trait NoteDispatch: Send + Sync {
    fn note(&self, seat: &SessionSlot, text: &str);
}

/// The note the first client's turn-on puts into the seat.
pub const LISTENING_NOTE: &str = "The user is listening to this seat.\n\nSpeak what they should hear as you would say it in a live conversation: send each message's substance through mcp__forge__voice__say as well as writing it in the chat, keeping the spoken form simpler and shorter so it lands by ear. A reply that is only written is one they will not hear. When the stop note arrives, you may stop speaking.";

/// What the session is told when the last client leaves.
pub const NOT_LISTENING_NOTE: &str =
    "The user has stopped listening to this seat.\n\nYou may stop speaking.";

/// The envelope a note rides, so a view draws it as information rather than
/// as the reader's own turn.
fn note_envelope(text: &str) -> String {
    format!("[Voice]\n\n{text}")
}

type Loader = Arc<dyn Fn() -> Result<Arc<dyn Engine>, String> + Send + Sync>;

/// The per-seat voice service. One render queue per seat, so a second `say`
/// waits behind the first, and one engine for the whole process, loaded at
/// boot and kept for the process's life; a failed warm is retried by the
/// first `say`.
#[derive(Clone)]
pub struct VoiceService {
    loader: Loader,
    engine: Arc<tokio::sync::Mutex<Option<Arc<dyn Engine>>>>,
    settings: VoiceSettings,
    sink: Arc<dyn AudioSink>,
    notes: Arc<dyn NoteDispatch>,
    seats: Arc<Mutex<HashMap<SessionSlot, Seat>>>,
    /// The previews one seat may have in flight: a held key updates the
    /// newest voice instead of queueing one render per press.
    previews: Arc<Mutex<HashMap<SessionSlot, PreviewSlot>>>,
}

#[derive(Default)]
struct PreviewSlot {
    latest: Option<String>,
    running: bool,
    /// The loop that owns `running`: a settle from an older loop must not
    /// clear a newer loop's flag or drop the voice it has yet to render.
    generation: u64,
    /// Bumped on every accepted press, so a chunk from a render the reader has
    /// already moved past is dropped rather than played.
    presses: u64,
}

struct Seat {
    listeners: BTreeSet<u64>,
    queue: mpsc::UnboundedSender<Job>,
    /// The listener count the render task reads, so it stops between
    /// sentences the moment the last one leaves rather than draining a
    /// queue nobody will hear.
    live: Arc<AtomicUsize>,
    /// The session has been told the user is listening and not yet told they
    /// stopped. A pause keeps it true, so a seat switch does not toggle the
    /// agent; an explicit off clears it.
    noted: bool,
}

struct Job {
    text: String,
    voice: Option<String>,
    engine: Arc<dyn Engine>,
    first: oneshot::Sender<Result<(), SayError>>,
    /// The seat's press count when this preview was taken up. A chunk from an
    /// older press is dropped rather than played: the reader has moved on.
    preview: Option<u64>,
}

impl VoiceService {
    pub fn new(
        loader: Loader,
        settings: VoiceSettings,
        sink: Arc<dyn AudioSink>,
        notes: Arc<dyn NoteDispatch>,
    ) -> Self {
        Self {
            loader,
            engine: Arc::new(tokio::sync::Mutex::new(None)),
            settings,
            sink,
            notes,
            seats: Arc::new(Mutex::new(HashMap::new())),
            previews: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Queue one utterance for `seat`. Answers once the first audio is
    /// rendered, so a named engine failure reaches the caller rather than
    /// disappearing into a playback queue.
    pub async fn say(&self, seat: &SessionSlot, text: String) -> Result<(), SayError> {
        if !self.settings.enabled {
            return Err(SayError::Disabled);
        }
        // Refuse before the load, so a seat nobody hears never pays it.
        self.listening_queue(seat)?;
        forge_tts::within_bound(&text, self.settings.max_utterance_seconds)
            .map_err(|error| SayError::TooLong(error.to_string()))?;

        let engine = self.engine().await?;
        // The listener can leave while the engine loads; ask again so the
        // say answers the empty seat rather than blaming the engine.
        let queue = self.listening_queue(seat)?;
        let (first, outcome) = oneshot::channel();
        let voice = self.settings.voice.clone();
        queue
            .send(Job { text, voice, engine, first, preview: None })
            .map_err(|_| SayError::NotReady)?;
        outcome.await.map_err(|_| SayError::NotReady)?
    }

    /// Render one fixed line in `voice` on `seat`'s own stream, so a voice is
    /// heard before it is chosen. The seat's speaker switch is the playback
    /// gate: nobody listening means nowhere to play.
    ///
    /// **One render loop per seat.** While a preview renders, a second press
    /// replaces the pending voice instead of queueing another render, so the
    /// newest press is the one heard and a held key costs one render.
    pub async fn preview(&self, seat: &SessionSlot, voice: String) -> Result<(), SayError> {
        if !self.settings.enabled {
            return Err(SayError::Disabled);
        }
        // Refuse before the load, so a seat nobody hears never pays it.
        self.listening_queue(seat)?;
        let mut generation = {
            let mut previews = self.previews.lock().map_err(|_| SayError::NotReady)?;
            let slot = previews.entry(seat.clone()).or_default();
            slot.latest = Some(voice);
            slot.presses += 1;
            if slot.running {
                return Ok(());
            }
            slot.running = true;
            slot.generation += 1;
            slot.generation
        };
        loop {
            let outcome = self.render_previews(seat).await;
            // The loop ends however it ends, and a press that landed after
            // its last take is still waiting: it is rendered here rather
            // than dropped by the settle.
            let Some(pending) = self.settle_previews(seat, generation) else {
                return outcome;
            };
            let Some(next) = self.restart_previews(seat, pending) else {
                return outcome;
            };
            generation = next;
        }
    }

    /// End the loop `generation` owns and hand back the voice a later press
    /// left waiting. A newer loop's flag and voice are its own, and neither
    /// is touched.
    fn settle_previews(&self, seat: &SessionSlot, generation: u64) -> Option<String> {
        let mut previews = self.previews.lock().ok()?;
        let slot = previews.get_mut(seat)?;
        if slot.generation != generation {
            return None;
        }
        slot.running = false;
        slot.latest.take()
    }

    /// Claim the seat for a waiting voice. `None` when a newer loop already
    /// holds it: that loop carries the newest press, so an older voice is the
    /// one to drop.
    fn restart_previews(&self, seat: &SessionSlot, voice: String) -> Option<u64> {
        let mut previews = self.previews.lock().ok()?;
        let slot = previews.get_mut(seat)?;
        if slot.running {
            return None;
        }
        slot.latest = Some(voice);
        slot.running = true;
        slot.generation += 1;
        Some(slot.generation)
    }

    /// Drain the seat's pending previews, newest last.
    async fn render_previews(&self, seat: &SessionSlot) -> Result<(), SayError> {
        loop {
            // The take and the running flag move under one lock, so a press
            // that lands as this loop exits either starts its own loop or is
            // the voice this one renders.
            let next = {
                let mut previews = self.previews.lock().map_err(|_| SayError::NotReady)?;
                let Some(slot) = previews.get_mut(seat) else {
                    return Ok(());
                };
                let next = slot.latest.take().map(|voice| (voice, slot.presses));
                if next.is_none() {
                    slot.running = false;
                }
                next
            };
            let Some((voice, presses)) = next else {
                return Ok(());
            };
            let engine = self.engine().await?;
            // The listener can leave while the engine loads; a preview with
            // nowhere to play is done rather than an error.
            let Ok(queue) = self.listening_queue(seat) else {
                return Ok(());
            };
            let (first, outcome) = oneshot::channel();
            queue
                .send(Job {
                    text: PREVIEW_LINE.to_owned(),
                    voice: Some(voice),
                    engine,
                    first,
                    preview: Some(presses),
                })
                .map_err(|_| SayError::NotReady)?;
            outcome.await.map_err(|_| SayError::NotReady)??;
        }
    }

    /// The queue of a seat that still has a listener. A `say` reads it again
    /// after a load, because the seat can empty while the engine loads.
    fn listening_queue(&self, seat: &SessionSlot) -> Result<mpsc::UnboundedSender<Job>, SayError> {
        let seats = self.seats.lock().map_err(|_| SayError::NotReady)?;
        match seats.get(seat) {
            Some(state) if !state.listeners.is_empty() => Ok(state.queue.clone()),
            _ => Err(SayError::NoListener),
        }
    }

    /// Turn one client's voice on or off for `seat`. The session is told on
    /// the first client in, and on the last client out only when the reader
    /// turned it off themselves: a move to another seat pauses the seat
    /// without toggling the agent.
    pub fn set_listening(&self, seat: &SessionSlot, client: u64, on: bool, explicit: bool) {
        let note = {
            let Ok(mut seats) = self.seats.lock() else {
                return;
            };
            if on {
                if let Some(state) = seats.get_mut(seat) {
                    let first = state.listeners.is_empty();
                    state.listeners.insert(client);
                    if first {
                        // The previous queue task stopped when the last
                        // listener left; a return gets a fresh one.
                        let live = Arc::new(AtomicUsize::new(1));
                        state.queue = self.spawn_queue(seat, Arc::clone(&live));
                        state.live = live;
                    } else {
                        state.live.fetch_add(1, Ordering::SeqCst);
                    }
                    if first && !state.noted {
                        state.noted = true;
                        Some(LISTENING_NOTE)
                    } else {
                        None
                    }
                } else {
                    let live = Arc::new(AtomicUsize::new(1));
                    let queue = self.spawn_queue(seat, Arc::clone(&live));
                    seats.insert(
                        seat.clone(),
                        Seat { listeners: BTreeSet::from([client]), queue, live, noted: true },
                    );
                    Some(LISTENING_NOTE)
                }
            } else {
                let Some(state) = seats.get_mut(seat) else {
                    return;
                };
                if !state.listeners.remove(&client) {
                    return;
                }
                if state.listeners.is_empty() {
                    state.live.store(0, Ordering::SeqCst);
                    if explicit {
                        state.noted = false;
                        seats.remove(seat);
                        Some(NOT_LISTENING_NOTE)
                    } else {
                        // A pause: the seat keeps its note, so a return does
                        // not tell the agent it came back.
                        None
                    }
                } else {
                    state.live.fetch_sub(1, Ordering::SeqCst);
                    None
                }
            }
        };
        if let Some(text) = note {
            self.notes.note(seat, text);
        }
    }

    /// Swap the loaded engine for one already built, so a model change keeps
    /// serving until the new engine is up.
    pub async fn replace_engine(&self, engine: Arc<dyn Engine>) {
        *self.engine.lock().await = Some(engine);
    }

    pub fn listeners(&self, seat: &SessionSlot) -> usize {
        self.seats
            .lock()
            .ok()
            .and_then(|seats| seats.get(seat).map(|state| state.listeners.len()))
            .unwrap_or(0)
    }

    /// A connection has gone: take it off every seat it was listening to,
    /// and answer the seats whose count changed. A socket that dies says no
    /// goodbye, so this teardown is what lowers the count.
    pub fn release(&self, client: u64) -> Vec<SessionSlot> {
        let (changed, emptied) = {
            let Ok(mut seats) = self.seats.lock() else {
                return Vec::new();
            };
            let mut changed = Vec::new();
            let mut emptied = Vec::new();
            for (seat, state) in seats.iter_mut() {
                if !state.listeners.remove(&client) {
                    continue;
                }
                changed.push(seat.clone());
                if state.listeners.is_empty() {
                    state.live.store(0, Ordering::SeqCst);
                    emptied.push(seat.clone());
                } else {
                    state.live.fetch_sub(1, Ordering::SeqCst);
                }
            }
            for seat in &emptied {
                seats.remove(seat);
            }
            (changed, emptied)
        };
        for seat in &emptied {
            self.notes.note(seat, NOT_LISTENING_NOTE);
        }
        changed
    }

    /// Whether `[tts] enabled` is set. A disabled service registers no tool
    /// and refuses every `say`.
    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    /// The seat's occupant changed under a listening client: tell the new
    /// session it is heard, because the note fires on the listener set's
    /// transitions and a `/new` or `/resume` leaves that set alone.
    pub fn occupant_changed(&self, seat: &SessionSlot) {
        let listening = self
            .seats
            .lock()
            .ok()
            .and_then(|seats| seats.get(seat).map(|state| !state.listeners.is_empty()))
            .unwrap_or(false);
        if listening {
            self.notes.note(seat, LISTENING_NOTE);
        }
    }

    /// Load the engine ahead of the first `say`. A failed load leaves the
    /// slot empty, so the first `say` retries it.
    pub(crate) async fn warm(&self) -> Result<(), SayError> {
        self.engine().await.map(|_held| ())
    }

    /// The engine, loading it on the first caller. The mutex is held across
    /// the load, so a second `say` waits for it instead of racing it.
    async fn engine(&self) -> Result<Arc<dyn Engine>, SayError> {
        let mut slot = self.engine.lock().await;
        if let Some(engine) = slot.as_ref() {
            return Ok(Arc::clone(engine));
        }
        let loader = Arc::clone(&self.loader);
        let result = tokio::task::spawn_blocking(move || loader())
            .await
            .map_err(|error| SayError::Engine(error.to_string()))?;
        let engine = result.map_err(SayError::Engine)?;
        *slot = Some(Arc::clone(&engine));
        Ok(engine)
    }

    /// The seat's render loop. It stops the moment the last listener leaves,
    /// so a queued utterance is not rendered into a seat nobody hears.
    fn spawn_queue(
        &self,
        seat: &SessionSlot,
        live: Arc<AtomicUsize>,
    ) -> mpsc::UnboundedSender<Job> {
        let (queue, mut jobs) = mpsc::unbounded_channel::<Job>();
        let sink = Arc::clone(&self.sink);
        let previews = Arc::clone(&self.previews);
        let seat = seat.clone();
        tokio::spawn(async move {
            let mut seq = 0_u32;
            while live.load(Ordering::SeqCst) > 0 {
                let Some(job) = jobs.recv().await else {
                    break;
                };
                if live.load(Ordering::SeqCst) == 0 {
                    break;
                }
                seq = render(job, &seat, sink.as_ref(), &live, seq, previews.as_ref()).await;
            }
        });
        queue
    }
}

impl crate::Workspace {
    /// The voice service, built on first use - the boot prewarm is the first
    /// caller when `[tts]` is on. Its sink and note dispatch hold a
    /// `Weak<Workspace>`, which is why this waits for an `Arc<Self>` rather
    /// than being built with the rest of the value.
    pub(crate) fn voice_service(self: &Arc<Self>) -> Arc<VoiceService> {
        self.voice
            .get_or_init(|| {
                let weak = Arc::downgrade(self);
                let loader: Loader = Arc::new(move || {
                    let workspace =
                        weak.upgrade().ok_or_else(|| "the workspace is gone".to_owned())?;
                    workspace.load_speaking_engine()
                });
                Arc::new(self.build_voice_service(loader))
            })
            .clone()
    }

    /// Load the speaking engine for the active variant.
    ///
    /// The configured voice is checked against the installed set before the
    /// model loads, so a bad id is refused by name at warm rather than
    /// silently replaced by the engine's own single-voice fallback.
    fn load_speaking_engine(&self) -> Result<Arc<dyn Engine>, String> {
        let files = self.speaking_files().ok_or_else(|| {
            "the speaking model is not installed; install it from the models page".to_owned()
        })?;
        let installed = files.installed_voices().map_err(|error| error.to_string())?;
        forge_tts::select_voice(self.config_arc().tts.voice.as_deref(), &installed)
            .map_err(|error| error.to_string())?;
        let engine = forge_tts::KokoroTts::load(&files).map_err(|error| error.to_string())?;
        Ok(Arc::new(engine) as Arc<dyn Engine>)
    }

    /// Load the speaking engine beside boot, so the first `say` pays no
    /// model load. One task per forge run, spawned rather than awaited; a
    /// failed load leaves the slot empty, which the first `say` fills.
    ///
    /// The slot is only ever filled, never emptied: a loaded engine is never
    /// taken away, and a `say` that arrives while this load is still running
    /// waits on the same lock rather than answering [`SayError::NotReady`].
    pub fn start_voice_prewarm(self: &Arc<Self>) {
        // The wave-asked signal, kept before the gate: it is what a test
        // reads on a fixture with `[tts]` off. The dispatch has its own
        // guard, which only stops a second load of the same engine.
        self.voice_prewarm_started.store(true, Ordering::Release);
        let settings = self.config_arc().tts.clone();
        if !settings.enabled {
            return;
        }
        if self.voice_prewarm_dispatched.swap(true, Ordering::AcqRel) {
            return;
        }
        let files = self.speaking_files();
        let model = files
            .as_ref()
            .map_or_else(|| "unresolved".to_owned(), |files| files.model.display().to_string());
        let dir = settings
            .models_dir()
            .map_or_else(|| "unresolved".to_owned(), |dir| dir.display().to_string());
        let service = self.voice_service();
        let started = std::time::Instant::now();
        let span = tracing::info_span!("voice_prewarm");
        tracing::info!(
            event_name = "voice_prewarm_starting",
            models_dir = %dir,
            model = %model,
            "loading the speaking engine"
        );
        tokio::spawn(
            async move {
                match service.warm().await {
                    Ok(()) => tracing::info!(
                        event_name = "voice_prewarm_ready",
                        elapsed_ms = started.elapsed().as_millis(),
                        model = %model,
                        "the speaking engine is resident; a say pays no load"
                    ),
                    Err(error) => tracing::warn!(
                        event_name = "voice_prewarm_failed",
                        model = %model,
                        failure = %error,
                        "the speaking engine did not load; the first say will retry"
                    ),
                }
            }
            .instrument(span),
        );
    }

    /// Whether speech is on, as the running system answers it: the built
    /// service's own flag when it exists, else the config that would build
    /// one. A save marked restart cannot move one side without the other.
    pub(crate) fn voice_enabled(&self) -> bool {
        self.voice.get().map_or_else(|| self.config_arc().tts.enabled, |service| service.enabled())
    }

    /// The clients with voice on for `seat`. Zero before the service has
    /// been built, which is the truth: nobody could have toggled it yet.
    pub fn voice_listeners(&self, seat: &SessionSlot) -> usize {
        self.voice.get().map_or(0, |service| service.listeners(seat))
    }

    /// The seat's occupant changed under a listening client: tell the new
    /// session it is heard. A no-op before the service exists, because
    /// nobody could have toggled voice on.
    pub fn voice_occupant_changed(&self, seat: &SessionSlot) {
        if let Some(service) = self.voice.get() {
            service.occupant_changed(seat);
        }
    }

    /// A connection has left: take it off every seat it was listening to and
    /// tell each seat's subscribers the count it left behind.
    pub fn voice_release(&self, client: u64) -> Vec<SessionSlot> {
        let Some(service) = self.voice.get() else {
            return Vec::new();
        };
        let changed = service.release(client);
        for seat in &changed {
            let listeners = u32::try_from(service.listeners(seat)).unwrap_or(u32::MAX);
            let _ = self
                .update_sender()
                .send(crate::SessionUpdate::VoiceModeState { key: seat.clone(), listeners });
        }
        changed
    }

    /// Put an already-built engine behind the service, so a test can drive a
    /// render without loading a model.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn install_voice_engine_for_test(self: &Arc<Self>, engine: Arc<dyn Engine>) {
        let loader: Loader = Arc::new(move || Ok(Arc::clone(&engine)));
        let _ = self.voice.set(Arc::new(self.build_voice_service(loader)));
    }

    /// Speak one utterance the way `voice__say` does, so a cross-crate test
    /// can drive a render without an agent calling the tool.
    #[cfg(any(test, feature = "test-helpers"))]
    pub async fn voice_say_for_test(
        self: &Arc<Self>,
        seat: &SessionSlot,
        text: String,
    ) -> Result<(), SayError> {
        self.voice_service().say(seat, text).await
    }

    fn build_voice_service(self: &Arc<Self>, loader: Loader) -> VoiceService {
        let config = self.config_arc();
        let settings = VoiceSettings {
            enabled: config.tts.enabled,
            max_utterance_seconds: config.tts.max_utterance_seconds,
            voice: config.tts.voice.clone(),
        };
        VoiceService::new(
            loader,
            settings,
            Arc::new(WorkspaceSink { workspace: Arc::downgrade(self) }),
            Arc::new(WorkspaceNotes { workspace: Arc::downgrade(self) }),
        )
    }
}

/// The sink a real workspace hands the service: every rendered clip crosses
/// the update stream to the clients subscribed to the seat.
struct WorkspaceSink {
    workspace: std::sync::Weak<crate::Workspace>,
}

impl AudioSink for WorkspaceSink {
    fn push(&self, seat: &SessionSlot, chunk: Chunk) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        // A seat that has ended has no one to speak to, even if a stale
        // listener remains.
        if workspace.domain_session_for(seat).is_none() {
            return;
        }
        let _ = workspace.update_sender().send(crate::SessionUpdate::VoiceAudio {
            key: seat.clone(),
            seq: chunk.seq,
            sample_rate: chunk.sample_rate,
            pcm: chunk.pcm,
        });
    }
}

/// The note dispatch a real workspace hands the service: the listener note
/// enters the seat as a forge prompt, drawn as its own row.
struct WorkspaceNotes {
    workspace: std::sync::Weak<crate::Workspace>,
}

impl NoteDispatch for WorkspaceNotes {
    fn note(&self, seat: &SessionSlot, text: &str) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let uuid = forge_sdk::request_id::next_prompt_id();
        let prose = note_envelope(text);
        if let Err(error) = workspace.dispatch_workspace_prompt_under(
            seat,
            prose.clone(),
            crate::PromptSource::Forge,
            uuid.clone(),
        ) {
            tracing::warn!(
                event_name = "voice_note_not_delivered",
                seat = %seat.display(),
                %error,
                "the listener note did not reach the seat; the agent will not know it is heard"
            );
            return;
        }
        // The CLI does not echo a prompt, so this display row is what the
        // reader's page draws: the same forged turn a delivery carries.
        let mut row = Message::display_only_user(prose, uuid);
        row.stamp_if_absent(&crate::install::rfc3339_now());
        let _ = workspace.update_sender().send(crate::SessionUpdate::ChatAppended {
            key: seat.clone(),
            msg: row,
            source: Some(crate::PromptSource::Forge),
        });
    }
}

async fn render(
    job: Job,
    seat: &SessionSlot,
    sink: &dyn AudioSink,
    live: &AtomicUsize,
    mut seq: u32,
    previews: &Mutex<HashMap<SessionSlot, PreviewSlot>>,
) -> u32 {
    let Job { text, voice, engine, first, preview } = job;
    let mut first = Some(first);
    for sentence in forge_tts::sentences(&text) {
        if live.load(Ordering::SeqCst) == 0 {
            return seq;
        }
        let mut chunks = engine.speak(SpeakRequest { text: sentence, voice: voice.clone() });
        while let Some(result) = chunks.recv().await {
            if live.load(Ordering::SeqCst) == 0 {
                return seq;
            }
            match result {
                Ok(chunk) => {
                    // A preview the reader has already pressed past is not
                    // played: the newest press is the one heard, and this
                    // chunk is the abandoned voice still finishing.
                    let stale = preview.is_some_and(|presses| {
                        previews.lock().map_or(true, |map| {
                            map.get(seat).is_none_or(|slot| slot.presses != presses)
                        })
                    });
                    if let Some(sender) = first.take() {
                        let _ = sender.send(Ok(()));
                    }
                    if !stale {
                        sink.push(seat, Chunk { seq, ..chunk });
                    }
                    seq = seq.wrapping_add(1);
                }
                Err(error) => {
                    tracing::warn!(
                        event_name = "voice_render_failed",
                        slot = %seat.display(),
                        %error,
                        "a sentence did not render; the rest of the utterance is dropped"
                    );
                    if let Some(sender) = first.take() {
                        let _ = sender.send(Err(SayError::Engine(error.to_string())));
                    }
                    return seq;
                }
            }
        }
    }
    if let Some(sender) = first {
        let _ = sender.send(Ok(()));
    }
    seq
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use forge_primitives::SessionSlot;
    use forge_tts::{Chunk, Engine, Error, SpeakRequest};
    use tokio::sync::mpsc;

    use super::{
        AudioSink, LISTENING_NOTE, NOT_LISTENING_NOTE, NoteDispatch, SayError, VoiceService,
        VoiceSettings, note_envelope,
    };

    fn seat() -> SessionSlot {
        SessionSlot::new("Org", "project", "lead")
    }

    struct RecordingSink {
        chunks: Mutex<Vec<(SessionSlot, Chunk)>>,
    }

    impl AudioSink for RecordingSink {
        fn push(&self, seat: &SessionSlot, chunk: Chunk) {
            self.chunks.lock().expect("unpoisoned").push((seat.clone(), chunk));
        }
    }

    struct RecordingNotes {
        said: Mutex<Vec<String>>,
    }

    impl NoteDispatch for RecordingNotes {
        fn note(&self, _seat: &SessionSlot, text: &str) {
            self.said.lock().expect("unpoisoned").push(text.to_owned());
        }
    }

    /// Records each request and answers one chunk, so a sink's order is the
    /// engine's order. The voice each request named is recorded too, so a test
    /// can say which press was rendered.
    struct StubEngine {
        requests: Arc<Mutex<Vec<String>>>,
        voices: Arc<Mutex<Vec<Option<String>>>>,
        gate: Option<Arc<tokio::sync::Semaphore>>,
    }

    impl StubEngine {
        fn new(requests: Arc<Mutex<Vec<String>>>) -> Self {
            Self { requests, voices: Arc::new(Mutex::new(Vec::new())), gate: None }
        }
    }

    impl Engine for StubEngine {
        fn speak(&self, request: SpeakRequest) -> mpsc::Receiver<Result<Chunk, Error>> {
            self.requests.lock().expect("unpoisoned").push(request.text);
            self.voices.lock().expect("unpoisoned").push(request.voice);
            let (sender, receiver) = mpsc::channel(2);
            let permit = self.gate.as_ref().map(|gate| gate.clone().acquire_owned());
            tokio::spawn(async move {
                if let Some(permit) = permit {
                    drop(permit.await);
                }
                let _ = sender
                    .send(Ok(Chunk { seq: 0, sample_rate: 24_000, pcm: vec![1, 2, 3] }))
                    .await;
            });
            receiver
        }
    }

    struct Fixture {
        service: VoiceService,
        sink: Arc<RecordingSink>,
        notes: Arc<RecordingNotes>,
        requests: Arc<Mutex<Vec<String>>>,
        loads: Arc<AtomicUsize>,
    }

    fn service_with(engine: impl Fn() -> StubEngine + Send + Sync + 'static) -> Fixture {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) });
        let notes = Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) });
        let loads = Arc::new(AtomicUsize::new(0));
        let factory = Arc::new(engine);
        let loader = {
            let factory = Arc::clone(&factory);
            let requests = Arc::clone(&requests);
            let loads = Arc::clone(&loads);
            move || {
                loads.fetch_add(1, Ordering::SeqCst);
                let mut engine = factory();
                engine.requests = Arc::clone(&requests);
                Ok(Arc::new(engine) as Arc<dyn Engine>)
            }
        };
        Fixture {
            service: VoiceService::new(
                Arc::new(loader),
                VoiceSettings::default(),
                Arc::clone(&sink) as Arc<dyn AudioSink>,
                Arc::clone(&notes) as Arc<dyn NoteDispatch>,
            ),
            sink,
            notes,
            requests,
            loads,
        }
    }

    async fn wait_for(chunks: &Arc<RecordingSink>, expected: usize) {
        for _ in 0..200 {
            if chunks.chunks.lock().expect("unpoisoned").len() >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the sink saw fewer than {expected} chunks");
    }

    #[tokio::test]
    async fn a_say_with_no_listener_is_refused_without_touching_the_engine() {
        let fixture = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new()))));
        let outcome = fixture.service.say(&seat(), "Hello.".to_owned()).await;
        assert_eq!(outcome, Err(SayError::NoListener));
        assert!(fixture.requests.lock().expect("unpoisoned").is_empty(), "the engine is untouched");
        assert_eq!(fixture.loads.load(Ordering::SeqCst), 0, "the 2.3 GB model is never loaded");
    }

    /// A preview is playback on a seat's own stream, so no listener is a
    /// refusal before the engine is touched.
    #[tokio::test]
    async fn a_preview_with_no_listener_is_refused_without_touching_the_engine() {
        let fixture = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new()))));
        let outcome = fixture.service.preview(&seat(), "af_heart".to_owned()).await;
        assert_eq!(outcome, Err(SayError::NoListener));
        assert!(fixture.requests.lock().expect("unpoisoned").is_empty());
        assert_eq!(fixture.loads.load(Ordering::SeqCst), 0);
    }

    /// **One preview loop per seat.** A render in flight swallows later
    /// presses into its pending voice, so a held key cannot queue a render
    /// per press; the newest press is the one heard after the one running.
    #[tokio::test]
    async fn previews_coalesce_to_the_newest_voice_per_seat() {
        struct Gated {
            voices: Arc<Mutex<Vec<String>>>,
            permits: Arc<tokio::sync::Semaphore>,
        }

        impl Engine for Gated {
            fn speak(&self, request: SpeakRequest) -> mpsc::Receiver<Result<Chunk, Error>> {
                self.voices.lock().expect("unpoisoned").push(request.voice.unwrap_or_default());
                let (sender, receiver) = mpsc::channel(2);
                let permits = Arc::clone(&self.permits);
                tokio::spawn(async move {
                    let permit = permits.acquire_owned().await;
                    drop(permit);
                    let _ =
                        sender.send(Ok(Chunk { seq: 0, sample_rate: 24_000, pcm: vec![1] })).await;
                });
                receiver
            }
        }

        let voices = Arc::new(Mutex::new(Vec::new()));
        let permits = Arc::new(tokio::sync::Semaphore::new(0));
        let loader: super::Loader = {
            let voices = Arc::clone(&voices);
            let permits = Arc::clone(&permits);
            Arc::new(move || {
                Ok(Arc::new(Gated { voices: Arc::clone(&voices), permits: Arc::clone(&permits) })
                    as Arc<dyn Engine>)
            })
        };
        let service = VoiceService::new(
            loader,
            VoiceSettings::default(),
            Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) }) as Arc<dyn AudioSink>,
            Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) }) as Arc<dyn NoteDispatch>,
        );
        let seat = seat();
        service.set_listening(&seat, 1, true, false);

        let running = tokio::spawn({
            let service = service.clone();
            let seat = seat.clone();
            async move { service.preview(&seat, "af_heart".to_owned()).await }
        });
        for _ in 0..200 {
            if !voices.lock().expect("unpoisoned").is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Both land while the first render is blocked; only the last is owed
        // a render of its own.
        service.preview(&seat, "bf_emma".to_owned()).await.expect("queued");
        service.preview(&seat, "am_adam".to_owned()).await.expect("queued");

        permits.add_permits(2);
        running.await.expect("the task joins").expect("the first preview renders");
        for _ in 0..200 {
            if voices.lock().expect("unpoisoned").len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert_eq!(
            voices.lock().expect("unpoisoned").clone(),
            vec!["af_heart".to_owned(), "am_adam".to_owned()],
            "the running voice, then the newest press - never the one it replaced",
        );
    }

    /// **The listener leaves while the engine loads.** The preview loop is
    /// already running when the load finishes with nowhere to play; the seat
    /// must come back to rest, so the next press renders instead of finding a
    /// loop that claims to be running and never runs.
    #[tokio::test]
    async fn a_listener_that_leaves_while_the_engine_loads_does_not_wedge_previews() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (started, began) = std::sync::mpsc::channel::<()>();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = Arc::new(Mutex::new(released));
        let loader: super::Loader = {
            let requests = Arc::clone(&requests);
            Arc::new(move || {
                let _ = started.send(());
                released.lock().expect("unpoisoned").recv().ok();
                Ok(Arc::new(StubEngine::new(Arc::clone(&requests))) as Arc<dyn Engine>)
            })
        };
        let service = VoiceService::new(
            loader,
            VoiceSettings::default(),
            Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) }) as Arc<dyn AudioSink>,
            Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) }) as Arc<dyn NoteDispatch>,
        );
        let seat = seat();
        service.set_listening(&seat, 1, true, false);

        let preview = tokio::spawn({
            let service = service.clone();
            let seat = seat.clone();
            async move { service.preview(&seat, "af_heart".to_owned()).await }
        });
        let mut loader_started = false;
        for _ in 0..200 {
            if began.try_recv().is_ok() {
                loader_started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(loader_started, "the loader starts");
        // The listener leaves while the load is still blocked; a pause keeps
        // the seat, so a return needs no new note.
        service.set_listening(&seat, 1, false, false);
        release.send(()).expect("the loader is released");

        preview.await.expect("the task joins").expect("the preview ends without an error");

        // A listener returns and the next press renders: the seat was not left
        // claiming a preview runs.
        service.set_listening(&seat, 1, true, false);
        service.preview(&seat, "bf_emma".to_owned()).await.expect("the preview renders");
        assert_eq!(requests.lock().expect("unpoisoned").len(), 1);
    }

    /// A press lands after a loop's last take but before that loop settles:
    /// the old settle must leave the newer loop's flag and pending voice
    /// alone, or the press is dropped and a third one starts a second loop.
    #[tokio::test]
    async fn a_stale_loop_does_not_settle_a_newer_press() {
        let service = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new())))).service;
        let seat = seat();
        {
            let mut previews = service.previews.lock().expect("unpoisoned");
            let slot = previews.entry(seat.clone()).or_default();
            slot.generation = 2;
            slot.running = true;
            slot.latest = Some("bf_emma".to_owned());
        }

        assert_eq!(service.settle_previews(&seat, 1), None, "a stale loop settles nothing");

        let previews = service.previews.lock().expect("unpoisoned");
        let slot = previews.get(&seat).expect("the slot");
        assert!(slot.running, "a stale loop's settle cleared the newer loop's flag");
        assert_eq!(slot.latest.as_deref(), Some("bf_emma"), "the newer press's voice was dropped");
    }

    /// A press lands while the first voice is still rendering: the abandoned
    /// render's chunk is dropped rather than played, so the reader hears only
    /// the voice they pressed last.
    #[tokio::test]
    async fn a_chunk_from_a_press_the_reader_moved_past_is_not_played() {
        struct Gated {
            requests: Arc<Mutex<Vec<Option<String>>>>,
            release: Arc<tokio::sync::Semaphore>,
        }
        impl Engine for Gated {
            fn speak(&self, request: SpeakRequest) -> mpsc::Receiver<Result<Chunk, Error>> {
                self.requests.lock().expect("unpoisoned").push(request.voice.clone());
                let (sender, receiver) = mpsc::channel(2);
                let release = Arc::clone(&self.release);
                let voice = request.voice;
                tokio::spawn(async move {
                    if voice.as_deref() == Some("af_heart") {
                        release.acquire().await.ok();
                    }
                    let pcm = if voice.as_deref() == Some("af_heart") { vec![1] } else { vec![2] };
                    let _ = sender.send(Ok(Chunk { seq: 0, sample_rate: 24_000, pcm })).await;
                });
                receiver
            }
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let sink = Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) });
        let loader: super::Loader = {
            let requests = Arc::clone(&requests);
            let release = Arc::clone(&release);
            Arc::new(move || {
                Ok(Arc::new(Gated {
                    requests: Arc::clone(&requests),
                    release: Arc::clone(&release),
                }) as Arc<dyn Engine>)
            })
        };
        let service = VoiceService::new(
            loader,
            VoiceSettings::default(),
            Arc::clone(&sink) as Arc<dyn AudioSink>,
            Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) }) as Arc<dyn NoteDispatch>,
        );
        let seat = seat();
        service.set_listening(&seat, 1, true, false);

        let first = tokio::spawn({
            let service = service.clone();
            let seat = seat.clone();
            async move { service.preview(&seat, "af_heart".to_owned()).await }
        });
        let mut started = false;
        for _ in 0..200 {
            if requests.lock().expect("unpoisoned").as_slice() == [Some("af_heart".to_owned())] {
                started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(started, "the first voice renders");
        service.preview(&seat, "bf_emma".to_owned()).await.expect("the second press is accepted");
        release.add_permits(1);
        first.await.expect("the task joins").expect("the preview ends");

        wait_for(&sink, 1).await;
        assert_eq!(
            sink.chunks
                .lock()
                .expect("unpoisoned")
                .iter()
                .map(|(_, chunk)| chunk.pcm.clone())
                .collect::<Vec<_>>(),
            vec![vec![2]],
            "only the newest press is heard",
        );
        assert_eq!(requests.lock().expect("unpoisoned").len(), 2, "both voices rendered");
    }

    /// A press lands while the loop is loading an engine that then fails. The
    /// loop's settle must hand that voice to a fresh loop rather than clear
    /// it: the caller of the second press already got its `Ok`.
    #[tokio::test]
    async fn a_press_during_a_failing_load_is_rendered_rather_than_dropped() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let voices = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let (started, began) = std::sync::mpsc::channel::<()>();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = Arc::new(Mutex::new(released));
        let loader: super::Loader = {
            let requests = Arc::clone(&requests);
            let voices = Arc::clone(&voices);
            let calls = Arc::clone(&calls);
            Arc::new(move || {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    let _ = started.send(());
                    released.lock().expect("unpoisoned").recv().ok();
                    return Err("the first load dies".to_owned());
                }
                Ok(Arc::new(StubEngine {
                    requests: Arc::clone(&requests),
                    voices: Arc::clone(&voices),
                    gate: None,
                }) as Arc<dyn Engine>)
            })
        };
        let service = VoiceService::new(
            loader,
            VoiceSettings::default(),
            Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) }) as Arc<dyn AudioSink>,
            Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) }) as Arc<dyn NoteDispatch>,
        );
        let seat = seat();
        service.set_listening(&seat, 1, true, false);

        let first = tokio::spawn({
            let service = service.clone();
            let seat = seat.clone();
            async move { service.preview(&seat, "af_heart".to_owned()).await }
        });
        let mut loader_started = false;
        for _ in 0..200 {
            if began.try_recv().is_ok() {
                loader_started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(loader_started, "the loader starts");
        service.preview(&seat, "bf_emma".to_owned()).await.expect("the press is accepted");
        release.send(()).expect("the loader is released");

        first.await.expect("the task joins").expect("the waiting voice renders");
        assert_eq!(
            voices.lock().expect("unpoisoned").as_slice(),
            &[Some("bf_emma".to_owned())],
            "the press during the failing load is the voice rendered",
        );
    }

    #[tokio::test]
    async fn a_disabled_service_refuses_a_say() {
        let fixture = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new()))));
        let service = VoiceService::new(
            Arc::new(|| Err("no model loads in this test".to_owned())),
            VoiceSettings { enabled: false, ..VoiceSettings::default() },
            Arc::clone(&fixture.sink) as Arc<dyn AudioSink>,
            Arc::clone(&fixture.notes) as Arc<dyn NoteDispatch>,
        );
        service.set_listening(&seat(), 1, true, false);
        assert_eq!(service.say(&seat(), "Hello.".to_owned()).await, Err(SayError::Disabled));
    }

    #[tokio::test]
    async fn a_chunk_error_reaches_the_caller() {
        struct Failing;
        impl Engine for Failing {
            fn speak(&self, _request: SpeakRequest) -> mpsc::Receiver<Result<Chunk, Error>> {
                let (sender, receiver) = mpsc::channel(1);
                tokio::spawn(async move {
                    let _ = sender.send(Err(Error::Render("the render died".to_owned()))).await;
                });
                receiver
            }
        }
        let sink = Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) });
        let notes = Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) });
        let service = VoiceService::new(
            Arc::new(|| Ok(Arc::new(Failing) as Arc<dyn Engine>)),
            VoiceSettings::default(),
            Arc::clone(&sink) as Arc<dyn AudioSink>,
            Arc::clone(&notes) as Arc<dyn NoteDispatch>,
        );
        service.set_listening(&seat(), 1, true, false);
        let outcome = service.say(&seat(), "Hello.".to_owned()).await;
        assert!(
            matches!(&outcome, Err(SayError::Engine(reason)) if reason.contains("the render died")),
            "the caller hears the engine's own failure, got: {outcome:?}",
        );
    }

    #[tokio::test]
    async fn the_queue_stops_when_the_last_listener_leaves() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) });
        let notes = Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) });
        let engine = {
            let mut engine = StubEngine::new(Arc::clone(&requests));
            engine.gate = Some(Arc::clone(&gate));
            Arc::new(engine) as Arc<dyn Engine>
        };
        let service = VoiceService::new(
            Arc::new(move || Ok(Arc::clone(&engine))),
            VoiceSettings::default(),
            Arc::clone(&sink) as Arc<dyn AudioSink>,
            Arc::clone(&notes) as Arc<dyn NoteDispatch>,
        );
        service.set_listening(&seat(), 1, true, false);
        let first = tokio::spawn({
            let service = service.clone();
            async move { service.say(&seat(), "One.".to_owned()).await }
        });
        let second = tokio::spawn({
            let service = service.clone();
            async move { service.say(&seat(), "Two.".to_owned()).await }
        });
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(requests.lock().expect("unpoisoned").clone(), vec!["One.".to_owned()]);

        // The last listener leaves while the first render is held: the
        // queued utterance must not render into a seat nobody hears.
        service.set_listening(&seat(), 1, false, false);
        gate.add_permits(2);
        let _ = first.await.expect("joined");
        let _ = second.await.expect("joined");
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            requests.lock().expect("unpoisoned").clone(),
            vec!["One.".to_owned()],
            "the queued utterance never starts",
        );
        assert!(sink.chunks.lock().expect("unpoisoned").is_empty(), "nothing reaches the sink");
    }

    #[tokio::test]
    async fn an_occupant_change_tells_the_new_session_it_is_heard() {
        let fixture = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new()))));
        fixture.service.set_listening(&seat(), 1, true, false);
        fixture.service.occupant_changed(&seat());
        assert_eq!(
            fixture.notes.said.lock().expect("unpoisoned").clone(),
            vec![LISTENING_NOTE.to_owned(), LISTENING_NOTE.to_owned()],
            "the new occupant is told again",
        );
    }

    #[tokio::test]
    async fn two_sentences_reach_the_sink_in_order() {
        let fixture = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new()))));
        fixture.service.set_listening(&seat(), 1, true, false);
        fixture.service.say(&seat(), "One. Two.".to_owned()).await.expect("queued");
        wait_for(&fixture.sink, 2).await;
        let requests = fixture.requests.lock().expect("unpoisoned").clone();
        assert_eq!(requests, vec!["One.".to_owned(), "Two.".to_owned()]);
        let seqs: Vec<u32> = fixture
            .sink
            .chunks
            .lock()
            .expect("unpoisoned")
            .iter()
            .map(|(_, chunk)| chunk.seq)
            .collect();
        assert_eq!(seqs, vec![0, 1], "the sink hears the sentences in render order");
    }

    #[tokio::test]
    async fn an_utterance_past_the_bound_is_refused_by_name() {
        let fixture = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new()))));
        fixture.service.set_listening(&seat(), 1, true, false);
        let long = "word ".repeat(2_000);
        let outcome = fixture.service.say(&seat(), long).await;
        assert!(
            matches!(outcome, Err(SayError::TooLong(message)) if message.contains("max_utterance_seconds"))
        );
    }

    #[tokio::test]
    async fn the_listener_note_is_injected_once_for_the_first_client_and_withdrawn_for_the_last() {
        let fixture = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new()))));
        let said = |fixture: &Fixture| fixture.notes.said.lock().expect("unpoisoned").clone();

        fixture.service.set_listening(&seat(), 1, true, false);
        assert_eq!(said(&fixture), vec![LISTENING_NOTE.to_owned()]);
        fixture.service.set_listening(&seat(), 2, true, false);
        assert_eq!(
            said(&fixture),
            vec![LISTENING_NOTE.to_owned()],
            "the second client says nothing"
        );
        fixture.service.set_listening(&seat(), 1, false, false);
        assert_eq!(said(&fixture).len(), 1, "one client is still listening");
        fixture.service.set_listening(&seat(), 2, false, true);
        assert_eq!(said(&fixture), vec![LISTENING_NOTE.to_owned(), NOT_LISTENING_NOTE.to_owned()],);
        assert_eq!(fixture.service.listeners(&seat()), 0);
    }

    #[test]
    fn a_note_rides_the_envelope_a_view_draws_as_information() {
        assert_eq!(
            note_envelope(LISTENING_NOTE),
            format!("[Voice]\n\n{LISTENING_NOTE}"),
            "the note is information for the reader and a message for the agent",
        );
        assert_eq!(
            note_envelope(NOT_LISTENING_NOTE),
            format!("[Voice]\n\n{NOT_LISTENING_NOTE}"),
            "the withdrawal rides the same envelope",
        );
    }

    #[test]
    fn the_listening_note_carries_the_conversational_register() {
        assert!(
            LISTENING_NOTE
                .contains("Speak what they should hear as you would say it in a live conversation"),
            "the note names the register",
        );
        assert!(
            LISTENING_NOTE.contains("send each message's substance through mcp__forge__voice__say"),
            "the note names the tool that makes it heard",
        );
        assert!(
            LISTENING_NOTE
                .contains("keeping the spoken form simpler and shorter so it lands by ear"),
            "the spoken form is shorter than the page",
        );
        assert!(
            LISTENING_NOTE.contains("A reply that is only written is one they will not hear"),
            "a written-only reply is not heard",
        );
        assert!(
            LISTENING_NOTE.contains("When the stop note arrives, you may stop speaking."),
            "the note releases the agent when listening stops",
        );
    }

    #[test]
    fn the_stop_note_keeps_its_title_and_releases_the_agent() {
        assert_eq!(
            NOT_LISTENING_NOTE,
            "The user has stopped listening to this seat.\n\nYou may stop speaking.",
            "the reader's row is titled by its first line and the agent is released",
        );
    }

    /// The boot prewarm's failure is not terminal: it leaves the slot empty,
    /// and the first `say` loads again.
    #[tokio::test]
    async fn a_failed_warm_is_retried_by_the_first_say() {
        let loads = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let service = VoiceService::new(
            {
                let loads = Arc::clone(&loads);
                let requests = Arc::clone(&requests);
                Arc::new(move || {
                    if loads.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err("the first load failed".to_owned());
                    }
                    Ok(Arc::new(StubEngine::new(Arc::clone(&requests))) as Arc<dyn Engine>)
                })
            },
            VoiceSettings::default(),
            Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) }),
            Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) }),
        );

        let warm = service.warm().await;
        assert!(
            matches!(&warm, Err(SayError::Engine(reason)) if reason.contains("the first load failed")),
            "the prewarm's own failure names the load, got: {warm:?}",
        );

        service.set_listening(&seat(), 1, true, false);
        assert_eq!(
            service.say(&seat(), "Hello.".to_owned()).await,
            Ok(()),
            "the first say retries the failed load",
        );
        assert_eq!(
            loads.load(Ordering::SeqCst),
            2,
            "the load ran twice: the failed warm, then the say",
        );
    }

    #[tokio::test]
    async fn a_paused_last_listener_says_nothing() {
        let fixture = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new()))));
        let said = |fixture: &Fixture| fixture.notes.said.lock().expect("unpoisoned").clone();

        fixture.service.set_listening(&seat(), 1, true, false);
        assert_eq!(said(&fixture), vec![LISTENING_NOTE.to_owned()]);

        // A seat change is a pause, not a goodbye: the last listener leaves
        // without an explicit off, so the agent is not told the user stopped.
        fixture.service.set_listening(&seat(), 1, false, false);
        assert_eq!(said(&fixture), vec![LISTENING_NOTE.to_owned()], "a pause fires no note");
        assert_eq!(fixture.service.listeners(&seat()), 0);

        // The note survives the pause, so a return does not tell the agent
        // it came back.
        fixture.service.set_listening(&seat(), 1, true, false);
        assert_eq!(
            said(&fixture),
            vec![LISTENING_NOTE.to_owned()],
            "a return after a pause says nothing",
        );
    }

    #[tokio::test]
    async fn a_dropped_connection_leaves_every_seat_it_listened_to() {
        let fixture = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new()))));
        let first = seat();
        let second = SessionSlot::new("Org", "project", "worker");
        fixture.service.set_listening(&first, 1, true, false);
        fixture.service.set_listening(&first, 2, true, false);
        fixture.service.set_listening(&second, 2, true, false);

        // A socket dies without a goodbye: the core's teardown is the only
        // thing that can take its listener off, and it must leave every seat
        // the connection was on, not just the first one it finds.
        let changed = fixture.service.release(2);
        assert_eq!(changed.len(), 2, "both seats' counts moved");
        assert!(changed.contains(&first) && changed.contains(&second), "got: {changed:?}");
        assert_eq!(fixture.service.listeners(&first), 1);
        assert_eq!(fixture.service.listeners(&second), 0);
        assert_eq!(
            fixture.notes.said.lock().expect("unpoisoned").len(),
            3,
            "two listening notes and one withdrawal: the first seat still has a listener",
        );
        assert_eq!(
            fixture.notes.said.lock().expect("unpoisoned").last().map(String::as_str),
            Some(NOT_LISTENING_NOTE),
        );

        fixture.service.release(1);
        assert_eq!(fixture.service.listeners(&first), 0);
        assert_eq!(
            fixture.notes.said.lock().expect("unpoisoned").last().map(String::as_str),
            Some(NOT_LISTENING_NOTE),
        );
        assert!(fixture.service.release(1).is_empty(), "a second release changes nothing");
    }

    #[tokio::test]
    async fn a_second_say_queues_behind_the_first() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) });
        let notes = Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) });
        let engine = {
            let mut engine = StubEngine::new(Arc::clone(&requests));
            engine.gate = Some(Arc::clone(&gate));
            Arc::new(engine) as Arc<dyn Engine>
        };
        let service = VoiceService::new(
            Arc::new(move || Ok(Arc::clone(&engine))),
            VoiceSettings::default(),
            Arc::clone(&sink) as Arc<dyn AudioSink>,
            Arc::clone(&notes) as Arc<dyn NoteDispatch>,
        );
        service.set_listening(&seat(), 1, true, false);

        let first = tokio::spawn({
            let service = service.clone();
            async move { service.say(&seat(), "One.".to_owned()).await }
        });
        let second = tokio::spawn({
            let service = service.clone();
            async move { service.say(&seat(), "Two.".to_owned()).await }
        });
        // Let both reach the seat's queue while the first render is held.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            requests.lock().expect("unpoisoned").clone(),
            vec!["One.".to_owned()],
            "the second utterance has not started while the first renders",
        );

        gate.add_permits(2);
        first.await.expect("joined").expect("queued");
        second.await.expect("joined").expect("queued");
        wait_for(&sink, 2).await;
        assert_eq!(
            requests.lock().expect("unpoisoned").clone(),
            vec!["One.".to_owned(), "Two.".to_owned()],
        );
    }

    /// The engine is one instance for the process's life: the last listener
    /// leaving stops the queue, not the engine. The clock is advanced past
    /// the old idle window, which is the change that would take it.
    #[tokio::test(start_paused = true)]
    async fn the_engine_stays_loaded_after_the_last_listener_leaves() {
        struct Dropped(Arc<AtomicUsize>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct WarmEngine {
            _dropped: Dropped,
        }
        impl Engine for WarmEngine {
            fn speak(&self, _request: SpeakRequest) -> mpsc::Receiver<Result<Chunk, Error>> {
                let (sender, receiver) = mpsc::channel(1);
                tokio::spawn(async move {
                    let _ =
                        sender.send(Ok(Chunk { seq: 0, sample_rate: 24_000, pcm: vec![1] })).await;
                });
                receiver
            }
        }

        let loads = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let service = VoiceService::new(
            {
                let loads = Arc::clone(&loads);
                let drops = Arc::clone(&drops);
                Arc::new(move || {
                    loads.fetch_add(1, Ordering::SeqCst);
                    Ok(Arc::new(WarmEngine { _dropped: Dropped(Arc::clone(&drops)) })
                        as Arc<dyn Engine>)
                })
            },
            VoiceSettings::default(),
            Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) }),
            Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) }),
        );
        service.set_listening(&seat(), 1, true, false);
        service.say(&seat(), "One.".to_owned()).await.expect("rendered");
        assert_eq!(loads.load(Ordering::SeqCst), 1, "premise: the engine loaded");

        service.set_listening(&seat(), 1, false, true);
        // Let a release task reach its sleep before the clock jumps, or it
        // registers its deadline after the jump and the mutation survives.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(10 * 60)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(drops.load(Ordering::SeqCst), 0, "the engine outlives the last listener");
        assert!(
            service.engine.lock().await.is_some(),
            "the parked engine stays in the slot after the seat empties",
        );

        service.set_listening(&seat(), 1, true, false);
        service.say(&seat(), "Two.".to_owned()).await.expect("rendered");
        assert_eq!(loads.load(Ordering::SeqCst), 1, "the next say reuses the parked engine");
    }

    fn prewarm_workspace(tts_enabled: bool) -> (Arc<crate::Workspace>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.tts.enabled = tts_enabled;
        let (workspace, _updates) =
            crate::Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
                .expect("the stub builds");
        workspace.install_voice_engine_for_test(Arc::new(StubEngine::new(Arc::new(Mutex::new(
            Vec::new(),
        )))));
        (workspace, dir)
    }

    /// The boot prewarm loads with `[tts]` on: the stub engine reaches the
    /// slot. Deleting the dispatch or its spawn leaves this red, which the
    /// wave-asked guard alone cannot see.
    #[tokio::test]
    async fn the_boot_prewarm_loads_the_engine_when_tts_is_on() {
        let (workspace, _dir) = prewarm_workspace(true);
        workspace.start_voice_prewarm();
        assert!(workspace.test_voice_prewarm_started(), "the wave asked for it");

        let service = workspace.voice_service();
        for _ in 0..200 {
            if service.engine.lock().await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the boot prewarm did not load the engine");
    }

    /// The dispatch guard is not the wave-asked signal: a first call with
    /// `[tts]` off spends only the signal, and a second call still loads.
    #[tokio::test]
    async fn a_prewarm_call_after_tts_turns_on_dispatches() {
        let (workspace, _dir) = prewarm_workspace(false);
        workspace.start_voice_prewarm();
        assert!(workspace.test_voice_prewarm_started(), "the wave asked");
        assert!(
            workspace.voice_service().engine.lock().await.is_none(),
            "nothing loads while the section is off",
        );

        let mut enabled = crate::config::LoadedConfig::empty_for_test();
        enabled.tts.enabled = true;
        *workspace.config.write() = Arc::new(enabled);

        workspace.start_voice_prewarm();
        let service = workspace.voice_service();
        for _ in 0..200 {
            if service.engine.lock().await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the second call did not dispatch the load");
    }

    /// A listener can leave while the engine loads: the say answers the
    /// empty seat rather than a `NotReady` that blames the engine.
    #[tokio::test]
    async fn a_say_whose_listener_leaves_during_the_load_answers_no_listener() {
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let hold = Arc::new(Mutex::new(hold));
        let loads = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let service = VoiceService::new(
            {
                let hold = Arc::clone(&hold);
                let loads = Arc::clone(&loads);
                let requests = Arc::clone(&requests);
                Arc::new(move || {
                    loads.fetch_add(1, Ordering::SeqCst);
                    let _ = hold.lock().expect("unpoisoned").recv();
                    Ok(Arc::new(StubEngine::new(Arc::clone(&requests))) as Arc<dyn Engine>)
                })
            },
            VoiceSettings::default(),
            Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) }),
            Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) }),
        );
        service.set_listening(&seat(), 1, true, false);
        let say = tokio::spawn({
            let service = service.clone();
            async move { service.say(&seat(), "Hello.".to_owned()).await }
        });
        for _ in 0..200 {
            if loads.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "premise: the say is mid-load");

        service.set_listening(&seat(), 1, false, true);
        release.send(()).expect("the loader is waiting");
        assert_eq!(
            say.await.expect("joined"),
            Err(SayError::NoListener),
            "the seat emptied, so the say answers the seat, not the engine",
        );
    }

    /// A `say` that lands while the boot prewarm is still loading waits for
    /// that load: the engine is never released underneath a say, and a load
    /// in flight is not answered as `NotReady`.
    #[tokio::test]
    async fn a_say_waits_for_the_prewarm_instead_of_answering_not_ready() {
        // A dropped sender releases the load, so a failing assertion unwinds
        // instead of leaving the blocking loader parked for ever.
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let hold = Arc::new(Mutex::new(hold));
        let loads = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let service = VoiceService::new(
            {
                let hold = Arc::clone(&hold);
                let loads = Arc::clone(&loads);
                let requests = Arc::clone(&requests);
                Arc::new(move || {
                    loads.fetch_add(1, Ordering::SeqCst);
                    let _ = hold.lock().expect("unpoisoned").recv();
                    Ok(Arc::new(StubEngine::new(Arc::clone(&requests))) as Arc<dyn Engine>)
                })
            },
            VoiceSettings::default(),
            Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) }),
            Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) }),
        );
        service.set_listening(&seat(), 1, true, false);
        let warm = tokio::spawn({
            let service = service.clone();
            async move { service.warm().await }
        });
        for _ in 0..200 {
            if loads.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "premise: the prewarm is mid-load");

        let say = tokio::spawn({
            let service = service.clone();
            async move { service.say(&seat(), "Hello.".to_owned()).await }
        });
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(!say.is_finished(), "premise: the say is waiting on the load, not answered");

        release.send(()).expect("the loader is waiting");
        warm.await.expect("joined").expect("warmed");
        assert_eq!(
            say.await.expect("joined"),
            Ok(()),
            "the say waits for the prewarm instead of answering NotReady",
        );
        assert_eq!(loads.load(Ordering::SeqCst), 1, "one load served both");
    }

    #[tokio::test]
    async fn says_that_arrive_during_the_load_queue_behind_it() {
        let loads = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(RecordingSink { chunks: Mutex::new(Vec::new()) });
        let notes = Arc::new(RecordingNotes { said: Mutex::new(Vec::new()) });
        let service = VoiceService::new(
            {
                let loads = Arc::clone(&loads);
                let requests = Arc::clone(&requests);
                Arc::new(move || {
                    loads.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    Ok(Arc::new(StubEngine::new(Arc::clone(&requests))) as Arc<dyn Engine>)
                })
            },
            VoiceSettings::default(),
            Arc::clone(&sink) as Arc<dyn AudioSink>,
            Arc::clone(&notes) as Arc<dyn NoteDispatch>,
        );
        let seat = seat();
        service.set_listening(&seat, 1, true, false);

        let (first, second) = tokio::join!(
            service.say(&seat, "One.".to_owned()),
            service.say(&seat, "Two.".to_owned())
        );
        assert_eq!(first, Ok(()));
        assert_eq!(second, Ok(()));
        assert_eq!(loads.load(Ordering::SeqCst), 1, "the second say waits for the load");
        wait_for(&sink, 2).await;
    }
}
