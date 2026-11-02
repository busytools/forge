//! The per-seat voice service: listeners, the sentence queue, and synthesis.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_primitives::{Message, SessionSlot};
use forge_tts::{Chunk, Engine, SpeakRequest};
use tokio::sync::{mpsc, oneshot};

/// How long a loaded engine stays resident after the last listener leaves.
const IDLE_RELEASE: Duration = Duration::from_secs(5 * 60);

/// What one utterance renders with, from the `[tts]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSettings {
    pub enabled: bool,
    pub max_utterance_seconds: u64,
    pub language: Option<String>,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self { enabled: true, max_utterance_seconds: 120, language: None }
    }
}

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
pub const LISTENING_NOTE: &str = "The user is listening to this seat.\n\nAnything they should hear goes through mcp__forge__voice__say; the text written in the chat is not spoken.";

/// What the session is told when the last client leaves.
pub const NOT_LISTENING_NOTE: &str = "The user has stopped listening to this seat.";

/// The envelope a note rides, so a view draws it as information rather than
/// as the reader's own turn.
fn note_envelope(text: &str) -> String {
    format!("[Voice]\n\n{text}")
}

type Loader = Arc<dyn Fn() -> Result<Arc<dyn Engine>, String> + Send + Sync>;

/// The per-seat voice service. One render queue per seat, so a second `say`
/// waits behind the first, and one engine for the whole process, loaded on
/// the first utterance that needs it.
#[derive(Clone)]
pub struct VoiceService {
    loader: Loader,
    engine: Arc<tokio::sync::Mutex<Option<Arc<dyn Engine>>>>,
    settings: VoiceSettings,
    sink: Arc<dyn AudioSink>,
    notes: Arc<dyn NoteDispatch>,
    seats: Arc<Mutex<HashMap<SessionSlot, Seat>>>,
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
    engine: Arc<dyn Engine>,
    first: oneshot::Sender<Result<(), SayError>>,
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
        }
    }

    /// Queue one utterance for `seat`. Answers once the first audio is
    /// rendered, so a named engine failure reaches the caller rather than
    /// disappearing into a playback queue.
    pub async fn say(&self, seat: &SessionSlot, text: String) -> Result<(), SayError> {
        if !self.settings.enabled {
            return Err(SayError::Disabled);
        }
        let queue = {
            let seats = self.seats.lock().map_err(|_| SayError::NotReady)?;
            match seats.get(seat) {
                Some(state) if !state.listeners.is_empty() => state.queue.clone(),
                _ => return Err(SayError::NoListener),
            }
        };
        forge_tts::within_bound(&text, self.settings.max_utterance_seconds)
            .map_err(|error| SayError::TooLong(error.to_string()))?;

        let engine = self.engine().await?;
        let (first, outcome) = oneshot::channel();
        queue.send(Job { text, engine, first }).map_err(|_| SayError::NotReady)?;
        outcome.await.map_err(|_| SayError::NotReady)?
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
                    if first {
                        self.warm_engine();
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
                    self.warm_engine();
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
        if !on {
            self.schedule_idle_release();
        }
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
        if !emptied.is_empty() {
            self.schedule_idle_release();
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

    /// Load the engine in the background on the first listener, so the first
    /// `say` does not pay the load.
    fn warm_engine(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let warm = self.clone();
        handle.spawn(async move {
            if let Err(error) = warm.engine().await {
                tracing::warn!(
                    event_name = "voice_engine_warm_failed",
                    %error,
                    "the voice engine did not load; the first say will retry"
                );
            }
        });
    }

    /// Drop the engine once no seat has a listener and no render holds it.
    /// A new `say` loads it again, so the release costs a reload rather than
    /// leaving 2.3 GB resident for the process's life.
    fn schedule_idle_release(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let engine = Arc::clone(&self.engine);
        let seats = Arc::clone(&self.seats);
        handle.spawn(async move {
            tokio::time::sleep(IDLE_RELEASE).await;
            let quiet = {
                let Ok(seats) = seats.lock() else {
                    return;
                };
                seats.values().all(|state| state.listeners.is_empty())
            };
            if !quiet {
                return;
            }
            let mut slot = engine.lock().await;
            if slot.as_ref().is_some_and(|held| Arc::strong_count(held) == 1) {
                *slot = None;
            }
        });
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
        let settings = self.settings.clone();
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
                seq = render(job, &seat, &settings, sink.as_ref(), &live, seq).await;
            }
        });
        queue
    }
}

impl crate::Workspace {
    /// The voice service, built on first use. Its sink and note dispatch hold
    /// a `Weak<Workspace>`, which is why this waits for an `Arc<Self>` rather
    /// than being built with the rest of the value.
    pub(crate) fn voice_service(self: &Arc<Self>) -> Arc<VoiceService> {
        self.voice
            .get_or_init(|| {
                let config = self.config_arc();
                let files = config.tts.files();
                let loader: Loader = Arc::new(move || {
                    let files =
                        files.clone().ok_or_else(|| "no models directory resolves".to_owned())?;
                    let engine =
                        forge_tts::QwenTts::load(&files).map_err(|error| error.to_string())?;
                    Ok(Arc::new(engine) as Arc<dyn Engine>)
                });
                Arc::new(self.build_voice_service(loader))
            })
            .clone()
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
            language: config.tts.language.clone(),
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
            origin: None,
            source: Some(crate::PromptSource::Forge),
        });
    }
}

async fn render(
    job: Job,
    seat: &SessionSlot,
    settings: &VoiceSettings,
    sink: &dyn AudioSink,
    live: &AtomicUsize,
    mut seq: u32,
) -> u32 {
    let Job { text, engine, first } = job;
    let mut first = Some(first);
    for sentence in forge_tts::sentences(&text) {
        if live.load(Ordering::SeqCst) == 0 {
            return seq;
        }
        let mut chunks =
            engine.speak(SpeakRequest { text: sentence, language: settings.language.clone() });
        while let Some(result) = chunks.recv().await {
            if live.load(Ordering::SeqCst) == 0 {
                return seq;
            }
            match result {
                Ok(chunk) => {
                    if let Some(sender) = first.take() {
                        let _ = sender.send(Ok(()));
                    }
                    sink.push(seat, Chunk { seq, ..chunk });
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
    /// engine's order.
    struct StubEngine {
        requests: Arc<Mutex<Vec<String>>>,
        gate: Option<Arc<tokio::sync::Semaphore>>,
    }

    impl StubEngine {
        fn new(requests: Arc<Mutex<Vec<String>>>) -> Self {
            Self { requests, gate: None }
        }
    }

    impl Engine for StubEngine {
        fn speak(&self, request: SpeakRequest) -> mpsc::Receiver<Result<Chunk, Error>> {
            self.requests.lock().expect("unpoisoned").push(request.text);
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

    #[tokio::test]
    async fn the_first_listener_warms_the_engine() {
        let fixture = service_with(|| StubEngine::new(Arc::new(Mutex::new(Vec::new()))));
        fixture.service.set_listening(&seat(), 1, true, false);
        for _ in 0..200 {
            if fixture.loads.load(Ordering::SeqCst) == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the first listener did not warm the engine");
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
