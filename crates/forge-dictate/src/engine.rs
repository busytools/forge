//! The transcription engine and the tickets it hands back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, Once, PoisonError};
use std::time::{Duration, Instant};

use transcribe_cpp::{CancelToken, Feature, Model, RunOptions, Session};

use crate::audio::{AudioSource, SAMPLE_RATE};
use crate::diagnostics;
use crate::normalize::NormalizeOptions;
use crate::{Config, Error};

/// Native diagnostics are routed once per process, before any model is
/// loaded: unrouted, one load plus one inference writes 122 lines
/// straight to stderr and corrupts any host that owns it.
///
/// Routed to the `log` facade rather than silenced, so the output still
/// exists - but reaching it takes a `log`-to-tracing bridge the host
/// installs (`tracing_log::LogTracer`). Without one these records are
/// dropped, and this workspace does not install it today.
static ROUTE_NATIVE_LOGS: Once = Once::new();

/// Where the time went in one transcription.
///
/// Which stage moved is what separates "our code got slower" from "the
/// model changed".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stages {
    /// Loading weights. Zero on every run but the first, which is why it
    /// is separate: a cold start is not a regression.
    pub model_load: Duration,
    /// Turning the source into the buffer the model reads.
    pub resample: Duration,
    /// Mel spectrogram, as reported by the recognition runtime.
    pub mel: Duration,
    /// Encoder pass, as reported by the recognition runtime.
    pub encode: Duration,
    /// Decoder pass, as reported by the recognition runtime.
    pub decode: Duration,
    /// Rewriting recognition output into clean text. `None` when
    /// normalization did not happen - either none was configured, or it
    /// failed and the raw text was returned instead. A caller knows
    /// which, because it knows what it configured.
    pub normalize: Option<Duration>,
    /// How much audio this run consumed, so a caller can derive a
    /// realtime factor without knowing where the audio came from.
    pub audio: Duration,
}

/// Which window of a take is decoding, for a host that shows
/// transcription progress over a multi-window take. `window` counts
/// from 1; a single-window take reports once, at 1 of 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowProgress {
    pub window: usize,
    pub total: usize,
}

/// Clean text, plus what it was before normalization.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    /// The text a caller should use.
    ///
    /// Not necessarily one paragraph. Long input produces paragraph
    /// breaks even under the defaults, and
    /// [`crate::normalize::Structure::Lists`] and
    /// [`crate::normalize::Context::Email`] make multi-line output
    /// likely - bullets and greeting/sign-off blocks respectively. A
    /// fixed-height single-line surface will notice.
    pub text: String,
    /// Recognition output before normalization. Equal to `text` when no
    /// normalizer ran. Deliberately not named `raw`, which the
    /// recognition library already uses for something else.
    pub asr: String,
    /// Where the time went.
    pub stages: Stages,
    /// This is what fitted rather than what was said - either the
    /// recording hit [`Config::max_capture`], or the decode ran out of
    /// its output budget. Carried here rather than only logged: a host
    /// cannot offer to keep going from a log line it may not be routing.
    ///
    /// Which of the two it was depends on the path. Audio that arrived
    /// through [`Engine::transcribe`] or [`Engine::transcribe_with`] was
    /// never capped, so `true` there always means the decode budget. Only
    /// on the capture path can it be either, and there a host can compare
    /// [`Stages::audio`] against its configured cap.
    pub truncated: bool,
}

/// What a finished transcription produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Audio was heard and recognised.
    Transcript(Transcript),
    /// Nothing rose above [`Config::silence_floor`]. Distinct from an
    /// empty transcript on purpose: a muted input, an unplugged
    /// interface, the wrong device and a second process holding the
    /// microphone all land here, and a caller that cannot tell them from
    /// "you said nothing" has nothing to report.
    NoAudio {
        /// Loudest sample seen, in dBFS. The two shapes mean different
        /// things and a host should act on them differently.
        ///
        /// A finite value below the floor is a quiet room, a distant
        /// speaker, or a muted input - ordinary, and retrying may work.
        ///
        /// Negative infinity means every sample was exactly zero, which
        /// a live microphone does not produce. Measured: five captures
        /// of an empty room read -38.2 to -27.5 dBFS and never reached
        /// it, while a capture the operating system had refused produced
        /// nothing else. So it says something structural sits between
        /// the caller and the device - a denied permission, a hardware
        /// mute switch, a disconnected interface - rather than that
        /// nobody spoke. Which one is a host question; this crate
        /// reports the observation.
        ///
        /// Also measured: that condition is sticky within a process. A
        /// second capture behaved identically with no second prompt, so
        /// a host should surface it rather than retry in a loop.
        ///
        /// None of that applies to an [`crate::AudioSource`] that yielded
        /// no samples, which reaches the same value with no device in
        /// play at all.
        peak: f32,
        /// How much audio was heard, so a caller can say "four seconds of
        /// nothing" rather than just "nothing".
        audio: Duration,
    },
}

/// Rewrite recognition output, recording what it cost.
///
/// A normalizer that fails mid-session must not cost the speaker their
/// words: the raw text is returned and `stages.normalize` stays `None`,
/// which is the same shape as no normalizer being configured. A caller
/// knows which, because it knows what it configured.
fn normalize_text(
    normalizer: Option<&crate::normalize::Normalizer>,
    asr: &str,
    options: crate::normalize::NormalizeOptions,
    stages: &mut Stages,
) -> String {
    let Some(normalizer) = normalizer else { return asr.to_owned() };
    let started = Instant::now();
    match normalizer.normalize_with(asr, options) {
        Ok(clean) => {
            stages.normalize = Some(started.elapsed());
            clean
        }
        Err(error) => {
            tracing::warn!(%error, "normalization failed; returning raw text");
            asr.to_owned()
        }
    }
}

/// Samples to wall-clock at the one rate the models accept. Integer
/// maths so a long buffer cannot drift through a float cast.
fn audio_duration(samples: usize) -> Duration {
    Duration::from_micros((samples as u64).saturating_mul(1_000_000) / u64::from(SAMPLE_RATE))
}

/// Long takes are transcribed in windows rather than as one pass: the
/// recognition runtime decodes a whole buffer against one encoder
/// output, and past about two minutes the decoder derails into
/// repetition loops and skip-ahead re-syncs that drop mid-take audio
/// with nothing flagged. The ceiling is measured, not assumed: a
/// four-minute ten-paragraph take loses whole paragraphs at a 90 s
/// ceiling and lands complete at 60 s, so 60 is the shipping value.
const WINDOW_TARGET: usize = 60 * SAMPLE_RATE as usize;
/// Shortest a sought window may be, so pause-seeking cannot shred a
/// take into fragments too short to transcribe well. The take's final
/// window is exempt: it is whatever audio is left.
const WINDOW_MIN: usize = 30 * SAMPLE_RATE as usize;
/// 20 ms energy frames for pause-seeking.
const ENERGY_FRAME: usize = SAMPLE_RATE as usize / 50;

/// Sample ranges tiling `pcm`, each at most [`WINDOW_TARGET`] long,
/// cutting at the quietest [`ENERGY_FRAME`] block between
/// [`WINDOW_MIN`] and [`WINDOW_TARGET`] past the previous cut. A take
/// that fits one window comes back whole, which is the exact buffer
/// the recognition runtime saw before windowing existed.
///
/// Audio with no dip anywhere still splits, at the ceiling: the
/// windows tile exactly either way, so a word straddling such a cut
/// may transcribe loosely across the join but no audio is dropped.
fn window_bounds(pcm: &[f32]) -> Vec<(usize, usize)> {
    let mut bounds = Vec::new();
    let mut start = 0;
    while pcm.len() - start > WINDOW_TARGET {
        let lo = start + WINDOW_MIN;
        let hi = start + WINDOW_TARGET;
        let cut = lo + quietest_frame(&pcm[lo..hi]);
        bounds.push((start, cut));
        start = cut;
    }
    bounds.push((start, pcm.len()));
    bounds
}

/// Sample offset of the quietest [`ENERGY_FRAME`] block by summed
/// square. Ties go to the latest block, so a window runs as long as the
/// reliability ceiling allows and a flat pause cuts at its end. Zero
/// for a region shorter than one block.
fn quietest_frame(pcm: &[f32]) -> usize {
    let mut quietest = usize::MAX;
    let mut quietest_energy = f32::INFINITY;
    for (frame, block) in pcm.chunks_exact(ENERGY_FRAME).enumerate() {
        let energy: f32 = block.iter().map(|s| s * s).sum();
        if energy <= quietest_energy {
            quietest_energy = energy;
            quietest = frame;
        }
    }
    if quietest == usize::MAX { 0 } else { quietest * ENERGY_FRAME }
}

/// Window texts back into one transcript. Trims and drops empties, so
/// a silent stretch inside a take vanishes at the join instead of
/// leaving a doubled space.
fn join_window_texts(parts: &[String]) -> String {
    parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

/// The joined raw recognition, and the rewritten text over it, in that
/// order. The order is load-bearing: the normalizer reads the whole
/// take at once, so a sentence crossing a window boundary is repaired
/// with full context - normalizing per window would rewrite each
/// fragment without the context around the cut.
fn finalize_transcript(
    parts: &[String],
    normalize: impl FnOnce(&str) -> String,
) -> (String, String) {
    let asr = join_window_texts(parts);
    let text = normalize(&asr);
    (asr, text)
}

/// One queued transcription.
struct Job {
    pcm: Vec<f32>,
    resample: Duration,
    audio: Duration,
    truncated: bool,
    options: NormalizeOptions,
    cancel: CancelToken,
    progress: Option<Sender<WindowProgress>>,
    reply: Sender<Result<Outcome, Error>>,
}

/// How the weights ended up, once the worker has resolved them.
///
/// A failure is held as the parts of [`Error::ModelLoad`] rather than
/// the error itself, because every waiter needs its own copy and
/// `Error` is not `Clone`.
#[derive(Default)]
struct Readiness {
    outcome: Mutex<Option<Result<(), (PathBuf, String)>>>,
    settled: Condvar,
}

impl Readiness {
    fn settle(&self, outcome: Result<(), (PathBuf, String)>) {
        *self.outcome.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
        self.settled.notify_all();
    }

    fn wait(&self) -> Result<(), Error> {
        let mut outcome = self.outcome.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            match outcome.as_ref() {
                Some(Ok(())) => return Ok(()),
                Some(Err((path, message))) => {
                    return Err(Error::ModelLoad { path: path.clone(), message: message.clone() });
                }
                None => {
                    outcome = self.settled.wait(outcome).unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }
}

/// Loads the models and runs transcriptions, one at a time.
///
/// The recognition session needs `&mut self` to run, so one set of
/// weights means one caller at a time whatever the API looks like.
pub struct Engine {
    max_capture: Duration,
    device: Option<String>,
    normalize_options: NormalizeOptions,
    silence_floor: f32,
    /// Set before teardown. The worker checks it between jobs, so a
    /// backlog is DISCARDED rather than drained: shutdown should not wait
    /// out work whose callers are going away with it.
    stopping: Arc<AtomicBool>,
    /// The running job's cancel token, so teardown can abort an inference
    /// already in progress instead of waiting for it.
    in_flight: Arc<Mutex<Option<CancelToken>>>,
    /// `None` only while dropping.
    jobs: Option<Sender<Job>>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Label of whoever holds the microphone, if anyone.
    holder: Arc<Mutex<Option<String>>>,
    /// Answers [`Engine::wait_ready`] once the worker has the weights.
    readiness: Arc<Readiness>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Order matters. Dropping the sender alone does NOT end the loop:
        // `recv` keeps yielding buffered jobs and only errors once the
        // channel is empty, so a backlog would run to completion first.
        // Flag the stop, abort whatever is mid-inference, and only then
        // close the queue.
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(token) =
            self.in_flight.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref()
        {
            token.cancel();
        }
        self.jobs.take();

        // The join is what releases the weights before the process tears
        // down the native backend; without it ggml's Metal teardown trips
        // `GGML_ASSERT([rsets->data count] == 0)`. It does NOT cover a
        // load in progress: `Model::load` is a blocking FFI call with no
        // cancellation, so dropping an engine that has just started waits
        // the whole load out.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Engine {
    /// Load the configured models and start the worker.
    ///
    /// Blocking, like everything else here. Weights load on the worker
    /// thread, so this returns without waiting for them; a model that
    /// cannot be loaded surfaces from the first [`Ticket::recv`] rather
    /// than here, because that is where it is first used.
    pub fn new(cfg: Config) -> Result<Arc<Engine>, Error> {
        let dir = crate::fetch::models_dir(&cfg)?;
        let asr_path = dir.join(&cfg.asr_model.file);
        let max_capture = cfg.max_capture;
        let device = cfg.device.clone();
        let normalize_options = cfg.normalize_options;
        let silence_floor = cfg.silence_floor;
        let stopping = Arc::new(AtomicBool::new(false));
        let in_flight: Arc<Mutex<Option<CancelToken>>> = Arc::new(Mutex::new(None));
        let readiness = Arc::new(Readiness::default());
        let (jobs, queue) = channel();

        let handle = std::thread::Builder::new()
            .name("forge-dictate".into())
            .spawn({
                let stopping = Arc::clone(&stopping);
                let in_flight = Arc::clone(&in_flight);
                let readiness = Arc::clone(&readiness);
                move || worker(&asr_path, &cfg, &queue, &stopping, &in_flight, &readiness)
            })
            .map_err(|source| Error::WorkerSpawn { message: source.to_string() })?;

        Ok(Arc::new(Engine {
            max_capture,
            device,
            normalize_options,
            silence_floor,
            stopping,
            in_flight,
            jobs: Some(jobs),
            worker: Some(handle),
            holder: Arc::new(Mutex::new(None)),
            readiness,
        }))
    }

    /// Wait for the weights to be in memory.
    ///
    /// [`Engine::new`] hands the load to the worker and returns without
    /// it, so an engine over a model that cannot be read looks healthy
    /// until somebody speaks. This is how a caller learns which one it
    /// has before asking anyone to.
    ///
    /// Blocking, like everything else here. Returns once every
    /// configured model is loaded, or with the [`Error::ModelLoad`] a
    /// transcription would have failed with, naming the file. Idempotent
    /// and safe from several threads: the answer is kept and handed to
    /// each caller.
    pub fn wait_ready(&self) -> Result<(), Error> {
        self.readiness.wait()
    }

    /// The configured silence floor, in dBFS. A host drawing the
    /// capture level bar needs the same floor the silence decision
    /// uses, so the meter and the [`Outcome::NoAudio`] verdict agree
    /// by construction rather than by a host guessing the default.
    pub fn silence_floor(&self) -> f32 {
        self.silence_floor
    }

    /// Queue `source` for transcription.
    ///
    /// Rejects a source the models cannot read before queueing anything.
    /// A wrong sample rate or channel count is a caller mistake, knowable
    /// the moment the source is handed over, so it is refused here rather
    /// than arriving later mixed in with real runtime failures.
    pub fn transcribe(&self, source: impl AudioSource) -> Result<Ticket, Error> {
        self.transcribe_with(source, self.normalize_options)
    }

    /// Queue `source`, overriding the configured normalizer options for
    /// this transcription only. Styling is a per-recording choice, so it
    /// belongs on the call rather than on the engine.
    pub fn transcribe_with(
        &self,
        mut source: impl AudioSource,
        options: NormalizeOptions,
    ) -> Result<Ticket, Error> {
        if source.sample_rate() != SAMPLE_RATE {
            return Err(Error::SampleRate { expected: SAMPLE_RATE, actual: source.sample_rate() });
        }
        if source.channels() != 1 {
            return Err(Error::Channels { actual: source.channels() });
        }

        // Drained into one buffer because the recognition call takes the
        // whole signal contiguously. The chunking exists for the capture
        // path and for sources with no hardware behind them.
        let started = Instant::now();
        let mut pcm = Vec::new();
        while let Some(chunk) = source.next_chunk() {
            pcm.extend_from_slice(&chunk);
        }
        let resample = started.elapsed();

        self.submit(pcm, resample, false, options)
    }

    /// Take the microphone, labelling the holder so a competing caller
    /// can be told who has it.
    ///
    /// Exclusive WITHIN THIS PROCESS ONLY. A second process running its
    /// own engine is not arbitrated; the operating system decides, and
    /// both may record.
    pub fn try_capture(self: &Arc<Self>, holder: impl Into<String>) -> Result<Capture, Busy> {
        let mut lock = self.holder.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(held) = lock.as_deref() {
            return Err(Busy { holder: held.to_owned() });
        }
        *lock = Some(holder.into());
        drop(lock);

        let recording =
            Arc::new(crate::capture::Recording::new(crate::capture::sample_cap(self.max_capture)));
        let (ready, started) = channel();
        let max_capture = self.max_capture;
        let shared = Arc::clone(&recording);
        let wanted = self.device.clone();
        let recorder = std::thread::Builder::new()
            .name("forge-dictate-mic".into())
            .spawn(move || crate::capture::record(&shared, max_capture, wanted.as_deref(), &ready))
            .ok();

        // Carried on the capture rather than returned here: `Busy`
        // answers "who has the microphone", and a device that will not
        // open is a different question with a different answer.
        let failed_to_open = match started.recv() {
            Ok(Err(error)) => {
                tracing::warn!(%error, "input device did not open");
                Some(error)
            }
            Ok(Ok(())) => None,
            Err(_) => Some(Error::Capture { message: "the recorder thread did not start".into() }),
        };

        Ok(Capture {
            holder: Arc::clone(&self.holder),
            engine: Arc::clone(self),
            recording,
            recorder,
            max_capture,
            failed_to_open,
        })
    }

    /// Queue already-captured audio.
    fn submit(
        &self,
        pcm: Vec<f32>,
        resample: Duration,
        truncated: bool,
        options: NormalizeOptions,
    ) -> Result<Ticket, Error> {
        let (reply, answer) = channel();
        let (progress_tx, progress_rx) = channel();
        let cancel = CancelToken::new();

        // Silence is a property of the samples, so it is decided here
        // rather than on the worker: a quiet capture needs no weights,
        // should not load any, and should not queue behind a backlog.
        let peak = Self::peak_dbfs(&pcm);
        let audio = audio_duration(pcm.len());
        if peak < self.silence_floor {
            let _ = reply.send(Ok(Outcome::NoAudio { peak, audio }));
            return Ok(Ticket { answer, cancel, progress: None });
        }

        self.jobs
            .as_ref()
            .ok_or(Error::EngineStopped)?
            .send(Job {
                pcm,
                resample,
                audio,
                truncated,
                options,
                cancel: cancel.clone(),
                progress: Some(progress_tx),
                reply,
            })
            .map_err(|_| Error::EngineStopped)?;
        Ok(Ticket { answer, cancel, progress: Some(progress_rx) })
    }

    /// Loudest sample in `pcm`, in dBFS.
    fn peak_dbfs(pcm: &[f32]) -> f32 {
        let peak = pcm.iter().fold(0.0f32, |worst, s| worst.max(s.abs()));
        if peak <= 0.0 { f32::NEG_INFINITY } else { 20.0 * peak.log10() }
    }
}

/// Somebody else holds the microphone.
#[derive(Debug, Clone, thiserror::Error)]
#[error("the microphone is claimed by {holder}")]
pub struct Busy {
    /// Label the current holder passed to [`Engine::try_capture`].
    pub holder: String,
}

/// Records from the microphone, and holds the crate's claim on it until
/// dropped.
///
/// Release is tied to the value rather than to a method so a caller that
/// panics mid-capture cannot wedge the input for everyone else in the
/// process.
///
/// EXCLUSIVE WITHIN THIS PROCESS ONLY. A second process running its own
/// engine is not arbitrated - the operating system decides and both may
/// record - and forge's own single-instance lock is per config
/// directory, so several of its profiles can be running at once. That
/// contention is one of the cases [`Outcome::NoAudio`] exists to make
/// legible, because the loser typically records silence.
pub struct Capture {
    holder: Arc<Mutex<Option<String>>>,
    engine: Arc<Engine>,
    recording: Arc<crate::capture::Recording>,
    /// Taken by `finish`/`cancel`; otherwise joined by `Drop`.
    recorder: Option<std::thread::JoinHandle<()>>,
    max_capture: Duration,
    /// Why the input never opened, if it did not. Held rather than
    /// logged-and-forgotten: an empty buffer and a refused device both
    /// produce no samples, and reporting the second as
    /// [`Outcome::NoAudio`] would hide a denied permission behind a
    /// message about silence.
    failed_to_open: Option<Error>,
}

impl Capture {
    /// Loudest input since the last read, in dBFS. The read is
    /// take-and-reset - it answers "peak over the window you just
    /// polled" and clears, which is what a level meter drawing one bar
    /// per window wants; a read that held the all-time peak would
    /// freeze such a meter on the first syllable.
    ///
    /// Still a lock-free atomic, so it is safe to call from a render
    /// loop. The mutating read means two pollers steal windows from
    /// each other: one reader is the assumed caller.
    pub fn level(&self) -> f32 {
        self.recording.peak_dbfs()
    }

    /// Stop recording and queue what was captured. Releases the
    /// microphone before the transcription starts, so the next caller
    /// does not wait for inference.
    pub fn finish(self) -> Result<Ticket, Error> {
        let options = self.engine.normalize_options;
        self.finish_with(options)
    }

    /// Stop recording and queue what was captured, overriding the
    /// configured normalizer options for this recording only.
    pub fn finish_with(mut self, options: NormalizeOptions) -> Result<Ticket, Error> {
        if let Some(error) = self.failed_to_open.take() {
            return Err(error);
        }
        let pcm = self.stop_recording();
        let truncated = self.recording.was_truncated();
        if truncated {
            tracing::warn!(cap = ?self.max_capture, "capture reached its cap and stopped itself");
        }
        self.engine.submit(pcm, Duration::ZERO, truncated, options)
    }

    /// Stop recording and throw the audio away.
    pub fn cancel(self) {
        drop(self);
    }

    /// Why the input never opened, if it did not. Reading it at capture
    /// time is what lets a host refuse a dead device eagerly instead of
    /// running the level bar over a microphone that was never open and
    /// reporting the failure only when the caller lets go.
    pub fn open_error(&self) -> Option<&Error> {
        self.failed_to_open.as_ref()
    }

    /// Whether the capture reached [`Config::max_capture`] and stopped
    /// itself. A host polling the level reads this so it can submit the
    /// take instead of holding a microphone that is no longer running.
    pub fn was_truncated(&self) -> bool {
        self.recording.was_truncated()
    }

    /// Stop the recorder and join it, so the device is released before
    /// this returns rather than at some later point.
    fn stop_recording(&mut self) -> Vec<f32> {
        self.recording.stop();
        if let Some(recorder) = self.recorder.take() {
            let _ = recorder.join();
        }
        self.recording.take()
    }
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.holder.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("Capture").field("holder", &held.as_deref()).finish_non_exhaustive()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Device first, then the lock: releasing the label while the
        // stream is still open would let the next caller open a second
        // one against the same input.
        let _ = self.stop_recording();
        let mut lock = self.holder.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *lock = None;
    }
}

/// A transcription in flight.
pub struct Ticket {
    answer: Receiver<Result<Outcome, Error>>,
    cancel: CancelToken,
    progress: Option<Receiver<WindowProgress>>,
}

impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticket")
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl Ticket {
    /// Wait for the result.
    ///
    /// BLOCKING, and deliberately without an async twin: an async host
    /// wraps this in its own `spawn_blocking`, where the reverse would
    /// put a runtime in every consumer to serve one.
    pub fn recv(self) -> Result<Outcome, Error> {
        self.answer.recv().map_err(|_| Error::EngineStopped)?
    }

    /// A clone of this ticket's cancel token. Cancelling it aborts THIS
    /// transcription - a job still queued behind another takes the
    /// token with it and is aborted when its turn comes - which is why
    /// a host abandoning one take among several goes through here
    /// rather than through anything engine-wide.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Takes this ticket's per-window progress stream, if it has not
    /// been taken. Steps arrive before each window decodes; the stream
    /// closes when the job ends. A host that never takes it costs the
    /// worker a failed send per window and nothing more.
    pub fn take_progress(&mut self) -> Option<Receiver<WindowProgress>> {
        self.progress.take()
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Every configured model, in memory.
struct Loaded {
    model: Model,
    session: Session,
    normalizer: Option<crate::normalize::Normalizer>,
    /// What the recognition weights cost, for the first transcript's
    /// [`Stages`].
    model_load: Duration,
}

/// Load the recognition model, and the normalizer when one is
/// configured.
///
/// Failure is the parts of [`Error::ModelLoad`] rather than the error,
/// because the queue drain and every [`Engine::wait_ready`] caller each
/// need their own copy.
fn load_models(asr_path: &Path, cfg: &Config) -> Result<Loaded, (PathBuf, String)> {
    let started = Instant::now();
    let loaded = Model::load(asr_path).and_then(|model| model.session().map(|s| (model, s)));
    let model_load = started.elapsed();

    let loaded = loaded.and_then(|(model, session)| {
        // The rate is baked into the weights, so a model that wants a
        // different one does not fail - it silently transcribes worse.
        // Reading what the GGUF declares turns that into a refusal.
        let declared = model.capabilities().native_sample_rate;
        if declared == i32::try_from(SAMPLE_RATE).unwrap_or(i32::MAX) {
            Ok((model, session))
        } else {
            Err(transcribe_cpp::Error::ModelLoad(format!(
                "model expects {declared} Hz audio; this crate captures and feeds {SAMPLE_RATE} Hz"
            )))
        }
    });
    let (model, session) = loaded.map_err(|source| (asr_path.to_path_buf(), source.to_string()))?;

    // Loaded here, on the worker, so a second set of weights never lands
    // on the caller's thread.
    let normalizer = match cfg.normalizer.as_ref() {
        None => None,
        Some(spec) => {
            let path = asr_path.with_file_name(&spec.file);
            Some(
                crate::normalize::Normalizer::load(&path)
                    .map_err(|source| (path, source.to_string()))?,
            )
        }
    };

    Ok(Loaded { model, session, normalizer, model_load })
}

/// Owns the weights and drains the queue.
fn worker(
    asr_path: &Path,
    cfg: &Config,
    queue: &Receiver<Job>,
    stopping: &AtomicBool,
    in_flight: &Mutex<Option<CancelToken>>,
    readiness: &Readiness,
) {
    // Routed here rather than in `Engine::new` so the two cannot drift
    // apart: this is the only place a model is loaded, and suppression
    // has to precede that.
    ROUTE_NATIVE_LOGS.call_once(transcribe_cpp::init_logging);

    let loaded = load_models(asr_path, cfg);
    // Settled before the branch below can return, so a caller waiting on
    // the weights hears what a queued job would have been told.
    readiness.settle(match &loaded {
        Ok(_) => Ok(()),
        Err(failure) => Err(failure.clone()),
    });

    let Loaded { model, mut session, normalizer, model_load } = match loaded {
        Ok(loaded) => loaded,
        Err((path, message)) => {
            // Every waiting caller hears the same thing, rather than
            // blocking forever on a worker that never started.
            while let Ok(job) = queue.recv() {
                let _ = job
                    .reply
                    .send(Err(Error::ModelLoad { path: path.clone(), message: message.clone() }));
            }
            return;
        }
    };

    let cancellable = model.supports(Feature::Cancellation);
    if !cancellable {
        // Otherwise abandoning a ticket looks like it worked and silently
        // does not, with nothing in the log to explain the wait.
        tracing::info!(
            "this model does not honour cancellation; abandoning a ticket discards the result but does not stop the work"
        );
    }
    let mut options = RunOptions::default();
    options.language.clone_from(&cfg.language);
    let mut first = true;

    while let Ok(job) = queue.recv() {
        if stopping.load(Ordering::Relaxed) {
            let _ = job.reply.send(Err(Error::EngineStopped));
            continue;
        }
        let mut stages = Stages {
            model_load: if first { model_load } else { Duration::ZERO },
            resample: job.resample,
            audio: job.audio,
            ..Stages::default()
        };

        if cancellable {
            session.set_cancel_token(&job.cancel);
        }
        in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(job.cancel.clone());
        // Re-checked after installing the token, not only before: teardown
        // between the first check and this line would have seen `None` in
        // `in_flight`, cancelled nothing, and then waited out a full
        // inference it believed it had aborted.
        if stopping.load(Ordering::Relaxed) {
            job.cancel.cancel();
        }
        // Each window runs on its own against the shared session; a
        // window that outruns the decode budget keeps what it recognised
        // and hands the flag up, where today the whole rest of the take
        // would have been lost with it.
        let mut asr_parts: Vec<String> = Vec::new();
        let mut window_records: Vec<diagnostics::WindowRecord> = Vec::new();
        let mut truncated = job.truncated;
        let mut failure: Option<Error> = None;
        let started = Instant::now();
        let windows = window_bounds(&job.pcm);
        let total = windows.len();
        let window_ms =
            |samples: usize| u64::try_from(audio_duration(samples).as_millis()).unwrap_or(u64::MAX);
        for (k, &(start, end)) in windows.iter().enumerate() {
            if let Some(progress) = job.progress.as_ref() {
                let _ = progress.send(WindowProgress { window: k + 1, total });
            }
            match session.run(&job.pcm[start..end], &options) {
                Ok(out) => {
                    stages.mel = stages.mel.saturating_add(Duration::from_secs_f64(
                        f64::from(out.timings.mel_ms) / 1000.0,
                    ));
                    stages.encode = stages.encode.saturating_add(Duration::from_secs_f64(
                        f64::from(out.timings.encode_ms) / 1000.0,
                    ));
                    stages.decode = stages.decode.saturating_add(Duration::from_secs_f64(
                        f64::from(out.timings.decode_ms) / 1000.0,
                    ));
                    window_records.push(diagnostics::WindowRecord {
                        start_ms: window_ms(start),
                        end_ms: window_ms(end),
                        raw: out.text.clone(),
                    });
                    asr_parts.push(out.text);
                }
                // Discriminated on the ERROR VARIANT rather than on
                // `was_aborted`/`was_truncated`. Those report "the most
                // recent run", and `run` has early returns that never reach
                // native at all - an interior NUL in the language string, an
                // oversized buffer, a busy session - on which the flags still
                // hold the PREVIOUS job's value. Per-error state cannot go
                // stale.
                Err(transcribe_cpp::Error::Aborted { .. }) => {
                    failure = Some(Error::Cancelled);
                    break;
                }
                Err(transcribe_cpp::Error::OutputTruncated { partial: Some(partial), .. }) => {
                    window_records.push(diagnostics::WindowRecord {
                        start_ms: window_ms(start),
                        end_ms: window_ms(end),
                        raw: partial.text.clone(),
                    });
                    asr_parts.push(partial.text);
                    truncated = true;
                }
                Err(source) => {
                    failure = Some(Error::Recognition { message: source.to_string() });
                    break;
                }
            }
        }
        // The diagnostics record snapshots the stages, since the
        // transcript the answer carries takes the original.
        let diag_stages = stages.clone();
        let answer = if let Some(error) = failure {
            Err(error)
        } else {
            let (asr, text) = finalize_transcript(&asr_parts, |raw| {
                // A normalizer that fails mid-session must not cost the
                // speaker their words: fall back to the recognised text
                // and say so, where a load failure above is fatal.
                normalize_text(normalizer.as_ref(), raw, job.options, &mut stages)
            });
            // Consumed here rather than where `stages` is built: an
            // error or a cancel discards the stages, and taking the
            // load cost there would lose it for the process.
            first = false;
            Ok(Outcome::Transcript(Transcript { text, asr, stages, truncated }))
        };
        let diag = match &answer {
            Err(Error::Cancelled) | Ok(Outcome::NoAudio { .. }) => None,
            Err(_) => Some(("recognition_error", String::new())),
            Ok(Outcome::Transcript(transcript)) => {
                let label = if transcript.text.is_empty() { "empty" } else { "transcript" };
                Some((label, transcript.text.clone()))
            }
        };
        in_flight.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        let _ = job.reply.send(answer);

        // Best-effort diagnostics, after the take has landed: the reply
        // is never held back by a write, and an abandoned take (the
        // user's own cancel) keeps nothing. A queued take starts after
        // the write, which is the one cost a 30-minute capture's wav
        // ever asks anyone to pay.
        if let (Some(dir), Some((outcome, text))) = (&cfg.diagnostics_dir, diag)
            && !stopping.load(Ordering::Relaxed)
        {
            let record = diagnostics::TakeRecord {
                audio: &job.pcm,
                windows: &window_records,
                joined: &join_window_texts(&asr_parts),
                text: &text,
                stages: &diag_stages,
                processing_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                truncated,
                outcome,
            };
            diagnostics::capture_take(dir, diagnostics::take_stamp(), &record);
        }
    }
}

#[cfg(test)]
mod tests_engine {
    use super::*;
    use crate::{ConfigBuilder, Samples};

    /// An engine whose weights live in an empty directory. Enough for
    /// anything decided before a job is queued; a job would fail at
    /// `recv`, and these tests never queue one.
    fn engine_without_weights() -> (tempfile::TempDir, Arc<Engine>) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigBuilder::new().models_dir(dir.path()).normalizer(None).build();
        let engine = Engine::new(cfg).expect("an engine must start without waiting for weights");
        (dir, engine)
    }

    #[test]
    fn a_source_at_the_wrong_rate_is_refused_before_it_is_queued() {
        let (_dir, engine) = engine_without_weights();
        let err = engine
            .transcribe(Samples::new(vec![0.0; 16], 44_100, 1))
            .expect_err("44.1 kHz is not something the models can read");
        assert!(
            matches!(err, Error::SampleRate { expected: SAMPLE_RATE, actual: 44_100 }),
            "a wrong rate must be refused synchronously rather than resampled, got: {err:?}"
        );
    }

    #[test]
    fn a_stereo_source_is_refused_before_it_is_queued() {
        let (_dir, engine) = engine_without_weights();
        let err = engine
            .transcribe(Samples::new(vec![0.0; 16], SAMPLE_RATE, 2))
            .expect_err("interleaved stereo is not mono");
        assert!(
            matches!(err, Error::Channels { actual: 2 }),
            "stereo must be refused, not read as a doubled-rate mono signal, got: {err:?}"
        );
    }

    #[test]
    fn the_microphone_is_exclusive_and_names_who_holds_it() {
        let (_dir, engine) = engine_without_weights();
        let _held = engine.try_capture("first").expect("an idle microphone must be available");
        let busy = engine.try_capture("second").expect_err("a held microphone must refuse");
        assert_eq!(
            busy.holder, "first",
            "a refused caller must be told who holds it, or it cannot say anything useful"
        );
    }

    /// The floor a host meters against is the floor the silence verdict
    /// uses. A getter that drifted from the config would draw a bar and
    /// an outcome that disagree about where "nothing" starts.
    #[test]
    fn silence_floor_reads_the_configured_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .normalizer(None)
            .silence_floor(-3.0)
            .build();
        let engine = Engine::new(cfg).expect("engine must start");
        assert!(
            (engine.silence_floor() + 3.0).abs() < f32::EPSILON,
            "the getter must carry the configured floor, got {}",
            engine.silence_floor()
        );
    }

    /// On a machine with a working default input, a fresh capture is not
    /// carrying an open failure. Skipped where there is no audio stack,
    /// since there the failure arm is the only one reachable. Skipped too
    /// when the recorder cannot open a REAL default - the workspace-level
    /// refusal tests cover that arm without hardware.
    #[test]
    fn a_fresh_capture_over_a_working_device_carries_no_open_error() {
        let Ok(found) = crate::capture::devices() else { return };
        if !found.iter().any(|d| d.is_default) {
            return;
        }
        let (_dir, engine) = engine_without_weights();
        let capture = engine.try_capture("open-error").expect("an idle microphone must be held");
        assert!(
            capture.open_error().is_none(),
            "a capture over a working default input must not report an open failure: {:?}",
            capture.open_error().map(std::string::ToString::to_string)
        );
    }

    #[test]
    fn dropping_a_capture_releases_the_microphone() {
        let (_dir, engine) = engine_without_weights();
        let held = engine.try_capture("first").expect("an idle microphone must be available");
        drop(held);
        let again = engine.try_capture("second");
        assert!(
            again.is_ok(),
            "release must ride on Drop, or a panicking caller wedges the microphone for everyone"
        );
    }

    /// Cancelling a ticket must never wedge its own answer. With no
    /// weights every job resolves as a load failure, which is exactly
    /// the drain a caller abandoned mid-queue would have raced into.
    /// A no-op cancel would survive this pin in CI - the real-weights
    /// test below is what kills that mutation, where a cancelled job
    /// comes back cancelled instead of with words.
    #[test]
    fn cancelling_a_ticket_still_resolves_its_answer() {
        let (_dir, engine) = engine_without_weights();
        let ticket = engine.transcribe(Samples::mono(vec![0.6; 512])).expect("queued");
        let token = ticket.cancel_token();
        token.cancel();
        let answer =
            ticket.recv().expect_err("no weights means the job answers with the load failure");
        assert!(
            matches!(answer, Error::ModelLoad { .. }),
            "the cancelled job must still resolve, got: {answer:?}"
        );
    }

    /// A load failure has to reach a waiter, and it has to keep reaching
    /// them. Both halves are load-bearing for a host that gates its
    /// startup on this: settling `Ok` regardless of the load would let it
    /// proceed over a model that will fail on the first word, and an
    /// answer consumed by whoever asked first would hang the second
    /// caller forever.
    #[test]
    fn wait_ready_reports_the_model_that_would_not_load_to_every_caller() {
        let (dir, engine) = engine_without_weights();

        let first = engine.wait_ready().expect_err("an empty directory holds no weights");
        assert!(
            matches!(&first, Error::ModelLoad { path, .. } if path.starts_with(dir.path())),
            "readiness must carry the load failure and name the file it tried, got: {first:?}"
        );

        let again = engine.wait_ready().expect_err("the answer must outlive the first caller");
        assert!(
            matches!(again, Error::ModelLoad { .. }),
            "the outcome must be kept rather than consumed, got: {again:?}"
        );
    }

    /// The silence branch answers before the weights are touched, so it
    /// is reachable with no model on disk.
    #[test]
    fn a_silent_job_returns_no_audio_rather_than_an_empty_transcript() {
        let (_dir, engine) = engine_without_weights();
        let outcome = engine
            .transcribe(Samples::mono(vec![0.0; SAMPLE_RATE as usize * 2]))
            .expect("silence is still a valid 16 kHz mono source")
            .recv()
            .expect("a silent job must be answered, not fail");

        match outcome {
            Outcome::NoAudio { peak, audio } => {
                assert!(
                    peak.is_infinite() && peak.is_sign_negative(),
                    "digital silence must report no signal at all, got {peak}"
                );
                assert_eq!(
                    audio.as_secs(),
                    2,
                    "NoAudio must say how much nothing was heard, or it cannot be diagnosed"
                );
            }
            Outcome::Transcript(t) => {
                panic!("silence must not come back as a transcript: {t:?}")
            }
        }
    }

    /// Loud audio, so the silence short-circuit is not taken and the
    /// jobs genuinely reach the worker's queue. With no model on disk the
    /// load-failure drain answers both, which pins that every job
    /// resolves on its OWN reply channel and that neither hangs.
    #[test]
    fn every_queued_job_is_answered_even_when_the_model_is_missing() {
        let (_dir, engine) = engine_without_weights();
        // Loud, so neither is short-circuited as silence.
        let first = engine.transcribe(Samples::mono(vec![0.6; 512])).expect("queued");
        let second = engine.transcribe(Samples::mono(vec![0.6; 512])).expect("queued");

        for (nth, ticket) in [first, second].into_iter().enumerate() {
            let answer = ticket.recv();
            assert!(
                matches!(answer, Err(Error::ModelLoad { .. })),
                "queued job {nth} must be answered rather than left hanging, got: {answer:?}"
            );
        }
    }

    #[test]
    fn the_silence_floor_comes_from_the_config_not_a_constant() {
        let dir = tempfile::tempdir().unwrap();
        // A floor below digital silence can never be met, so even a loud
        // signal must be judged against the configured value.
        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .normalizer(None)
            .silence_floor(-3.0)
            .build();
        let engine = Engine::new(cfg).expect("engine must start");

        let outcome = engine
            .transcribe(Samples::mono(vec![0.25; SAMPLE_RATE as usize]))
            .expect("a valid source")
            .recv()
            .expect("must be answered");
        assert!(
            matches!(outcome, Outcome::NoAudio { .. }),
            "a -12 dBFS signal must fall below a -3 dBFS floor, so the config is what decides"
        );
    }
}

#[cfg(test)]
mod tests_windowing {
    use super::*;

    /// A take that fits one window must come back whole: that is the
    /// exact buffer the recognition runtime saw before windowing
    /// existed, and short takes must not move.
    #[test]
    fn a_take_that_fits_one_window_is_returned_whole() {
        for len in [0, 1, seconds(10), WINDOW_TARGET] {
            let pcm = vec![0.5; len];
            assert_eq!(
                window_bounds(&pcm),
                vec![(0, len)],
                "a take of {len} samples fits one window and must be returned undivided"
            );
        }
    }

    /// Every window respects the sizing bounds, the windows tile the
    /// take with no gap and no overlap, and only the final window may
    /// be shorter than the minimum.
    #[test]
    fn windows_stay_within_the_sizing_bounds_and_cover_everything() {
        // 200 s of speech with pauses, so both the pause-seeking and the
        // hard-cut paths are exercised across several windows.
        let pcm = with_silence(loud(200), seconds(40), seconds(44));
        let pcm = with_silence(pcm, seconds(85), seconds(88));

        let bounds = window_bounds(&pcm);
        assert!(!bounds.is_empty(), "a non-empty take gets at least one window");

        let mut expected_start = 0;
        for (i, &(start, end)) in bounds.iter().enumerate() {
            assert_eq!(start, expected_start, "window {i} must continue where the last ended");
            assert!(end > start, "window {i} must not be empty");
            assert!(end - start <= WINDOW_TARGET, "window {i} must not exceed the target size");
            let last = i + 1 == bounds.len();
            if !last {
                assert!(
                    end - start >= WINDOW_MIN,
                    "window {i} may only shrink past the minimum as the take's final window"
                );
            }
            expected_start = end;
        }
        assert_eq!(expected_start, pcm.len(), "the windows must cover the whole take");
    }

    /// A cut goes into the quietest stretch the search region offers:
    /// a pause between phrases, never a decoy quiet blip that sits
    /// before the region, and never mid-word while a pause is in range.
    #[test]
    fn cuts_land_in_the_quietest_stretch_of_the_search_region() {
        // A decoy pause before the first search region, the real pause
        // inside it, then more speech with a second pause inside the
        // next window's region.
        let pcm = with_silence(loud(200), seconds(20), seconds(20).saturating_add(seconds(1) / 2));
        let pcm = with_silence(pcm, seconds(40), seconds(44));
        let pcm = with_silence(pcm, seconds(85), seconds(88));

        let bounds = window_bounds(&pcm);
        assert!(
            bounds.len() >= 3,
            "a 200 s take over a 60 s target must split into several windows, got {bounds:?}"
        );
        let first_cut = bounds[0].1;
        assert!(
            (seconds(40)..seconds(44)).contains(&first_cut),
            "the first cut must land inside the pause at 40 s, got {first_cut}"
        );
        let second_cut = bounds[1].1;
        assert!(
            (seconds(85)..seconds(88)).contains(&second_cut),
            "the second cut must land inside the pause at 85 s, got {second_cut}"
        );
    }

    /// Speech with no pause anywhere still splits, hard-cutting just
    /// under the target: a cut mid-word beats a window long enough to
    /// derail.
    #[test]
    fn a_take_with_no_quiet_still_splits_hard_at_the_ceiling() {
        let pcm = loud(200);
        let bounds = window_bounds(&pcm);
        assert_eq!(bounds.len(), 4, "200 s over a 60 s target splits into four windows");
        for (i, &(start, end)) in bounds.iter().enumerate() {
            if i + 1 == bounds.len() {
                continue;
            }
            assert!(
                end - start >= WINDOW_TARGET - ENERGY_FRAME,
                "with no pause to seek, window {i} must run to the ceiling"
            );
        }
    }

    /// Window texts reassemble into one transcript without doubled
    /// spaces: a silent stretch inside a take transcribes to nothing
    /// and must vanish at the join rather than leave a mark.
    #[test]
    fn joining_drops_empty_windows_and_joins_the_rest_with_spaces() {
        let parts = vec!["alpha ".to_owned(), String::new(), " beta".to_owned()];
        assert_eq!(join_window_texts(&parts), "alpha beta", "empties vanish, edges trim");
        assert_eq!(join_window_texts(&["only".to_owned()]), "only", "one window joins to itself");
        assert_eq!(join_window_texts(&[String::new()]), "", "an all-empty take joins to nothing");
    }

    /// The normalizer must see the JOINED take exactly once. Asserted
    /// through the input it receives rather than through a counter:
    /// normalizing per window and joining after would hand it the last
    /// window alone, and a sentence crossing a window boundary would be
    /// rewritten without the context around the cut.
    #[test]
    fn the_normalizer_reads_the_joined_take_not_the_windows() {
        let parts = vec!["alpha and".to_owned(), " ".to_owned(), "beta continued".to_owned()];
        let mut seen = Vec::new();
        let (asr, text) = finalize_transcript(&parts, |raw| {
            seen.push(raw.to_owned());
            raw.replace("alpha and beta continued", "one flow")
        });
        assert_eq!(asr, "alpha and beta continued", "the raw join is the recognition output");
        assert_eq!(
            seen.as_slice(),
            ["alpha and beta continued"],
            "the rewriter must receive the whole joined take, once"
        );
        assert_eq!(text, "one flow", "the rewrite runs over the joined text");
    }

    fn seconds(s: usize) -> usize {
        s * SAMPLE_RATE as usize
    }

    fn loud(seconds_long: usize) -> Vec<f32> {
        vec![0.5; seconds(seconds_long)]
    }

    fn with_silence(mut pcm: Vec<f32>, from: usize, to: usize) -> Vec<f32> {
        pcm[from..to].fill(0.0);
        pcm
    }
}

/// Real recognition against the real weights.
///
/// Ignored by default because it needs a 1.5 GB model that CI does not
/// have. **A test that cannot run in CI looks like decoration right up
/// until it is the only thing that catches a class of defect**, and this
/// one already caught a process abort at exit that no unit test could
/// reach: only real weights hold Metal resources, and only holding them
/// races the teardown. Do not delete it for being unrunnable.
///
/// ```bash
/// cargo run -p forge-dictate --release --example fetch
/// cargo nextest run -p forge-dictate --release --run-ignored all -E 'test(transcribes)'
/// ```
#[cfg(test)]
mod tests_real_recognition {
    use super::*;
    use crate::{ConfigBuilder, Samples};

    fn read_wav(path: &std::path::Path) -> (Vec<f32>, u32) {
        let bytes = std::fs::read(path).expect("fixture must be readable");
        // Canonical 16-bit PCM: the fixtures are written that way and the
        // manifest's integrity gate keeps them that way.
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let data = bytes[44..]
            .chunks_exact(2)
            .map(|p| f32::from(i16::from_le_bytes([p[0], p[1]])) / 32768.0)
            .collect();
        (data, rate)
    }

    #[test]
    #[ignore = "needs the ASR weights; run with --run-ignored all after `--example fetch`"]
    fn transcribes_a_fixture_to_its_locked_baseline() {
        // Where the corpus lands with the fixtures work. Compile-time
        // constant, so the answer does not depend on where this runs.
        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/08_009s.wav");
        assert!(clip.exists(), "fixture missing at {}", clip.display());
        let (pcm, rate) = read_wav(&clip);

        let engine =
            Engine::new(ConfigBuilder::new().normalizer(None).build()).expect("engine must start");
        let outcome = engine
            .transcribe(Samples::new(pcm, rate, 1))
            .expect("a 16 kHz mono fixture must be accepted")
            .recv()
            .expect("recognition must succeed");

        let Outcome::Transcript(transcript) = outcome else {
            panic!("a clip of real speech must not read as silence: {outcome:?}");
        };
        assert_eq!(
            transcript.text,
            "But if there is a flag that we can pass to disallow these, that would be fantastic.",
            "recognition must still reproduce this clip's locked baseline"
        );
        assert!(
            transcript.stages.audio > Duration::from_secs(9),
            "the reported audio duration must match the clip, got {:?}",
            transcript.stages.audio
        );
    }

    /// The success arm of [`Engine::wait_ready`], which no test without
    /// weights can reach: with an empty models directory every waiter
    /// gets a failure, so settling `Err` unconditionally passes the unit
    /// test above and refuses to start over a perfectly good pair.
    #[test]
    #[ignore = "needs the ASR weights; run with --run-ignored all after `--example fetch`"]
    fn wait_ready_succeeds_before_anything_is_transcribed() {
        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/08_009s.wav");
        let (pcm, rate) = read_wav(&clip);

        let engine =
            Engine::new(ConfigBuilder::new().normalizer(None).build()).expect("engine must start");
        engine.wait_ready().expect("the configured weights are on disk and must load");

        let outcome = engine
            .transcribe(Samples::new(pcm, rate, 1))
            .expect("queued")
            .recv()
            .expect("recognition must succeed");
        assert!(
            matches!(outcome, Outcome::Transcript(_)),
            "readiness must mean the engine can transcribe, not merely that it answered: {outcome:?}"
        );
    }

    /// Two callers must each get their own words back.
    ///
    /// Crosstalk is structurally impossible today because every job
    /// carries its own reply channel - which is the reason to pin it. A
    /// later move to one shared channel keyed by id is a natural-looking
    /// optimisation and is exactly what would break this.
    #[test]
    #[ignore = "needs the ASR weights; run with --run-ignored all after `--example fetch`"]
    fn two_tickets_resolve_to_their_own_callers_text() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (first_pcm, rate) = read_wav(&dir.join("08_009s.wav"));
        let (second_pcm, _) = read_wav(&dir.join("04_005s.wav"));

        let engine =
            Engine::new(ConfigBuilder::new().normalizer(None).build()).expect("engine must start");
        // Both queued before either is read, so the worker holds two at once.
        let first = engine.transcribe(Samples::new(first_pcm, rate, 1)).expect("queued");
        let second = engine.transcribe(Samples::new(second_pcm, rate, 1)).expect("queued");

        let (Ok(Outcome::Transcript(a)), Ok(Outcome::Transcript(b))) =
            (first.recv(), second.recv())
        else {
            panic!("both jobs must produce transcripts");
        };
        assert!(
            a.text.contains("disallow"),
            "the first ticket must carry the first caller's audio, got: {}",
            a.text
        );
        assert!(
            b.text.contains("PC games"),
            "the second ticket must carry the second caller's audio, got: {}",
            b.text
        );
    }

    /// An abandoned ticket must not wedge the worker.
    ///
    /// The failure lands on the INNOCENT call: if dropping a ticket left
    /// the worker blocked on a receiver nobody holds, or never cleared
    /// `in_flight`, it is the next transcription that never returns.
    #[test]
    #[ignore = "needs the ASR weights; run with --run-ignored all after `--example fetch`"]
    fn dropping_a_ticket_does_not_wedge_the_next_one() {
        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/08_009s.wav");
        let (pcm, rate) = read_wav(&clip);

        let engine =
            Engine::new(ConfigBuilder::new().normalizer(None).build()).expect("engine must start");
        drop(engine.transcribe(Samples::new(pcm.clone(), rate, 1)).expect("queued"));

        let after = engine
            .transcribe(Samples::new(pcm, rate, 1))
            .expect("queued")
            .recv()
            .expect("the call after an abandoned one must still complete");
        assert!(
            matches!(after, Outcome::Transcript(_)),
            "an abandoned ticket must free the worker for the next caller, got: {after:?}"
        );
    }

    /// A cancelled ticket comes back cancelled, not with words.
    ///
    /// Cancelled before the worker's turn (the first job's model load
    /// gives the cancel a wide window), so this pins the whole path:
    /// token clone, install, and the abort mapping. Assumes the model
    /// honours cancellation - the engine logs a notice when it does
    /// not, and there this assert would misfire.
    #[test]
    #[ignore = "needs the ASR weights; run with --run-ignored all after `--example fetch`"]
    fn cancelling_a_ticket_returns_cancelled_rather_than_the_words() {
        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/08_009s.wav");
        let (pcm, rate) = read_wav(&clip);

        let engine =
            Engine::new(ConfigBuilder::new().normalizer(None).build()).expect("engine must start");
        let ticket = engine.transcribe(Samples::new(pcm, rate, 1)).expect("queued");
        ticket.cancel_token().cancel();
        let answer = ticket
            .recv()
            .expect_err("a cancelled ticket is an error, never the transcript it aborted");
        assert!(
            matches!(answer, Error::Cancelled),
            "cancelling before the turn must abort the job, got: {answer:?}"
        );
    }

    /// A per-call styling override must reach the model.
    ///
    /// Asserted as a DIFFERENCE against the default rather than against
    /// fixed text: the point is that the argument changes the output, and
    /// pinning exact wording would break on any model change while
    /// proving nothing extra.
    #[test]
    #[ignore = "needs both models; run with --run-ignored all after `--example fetch`"]
    fn a_styling_override_changes_the_text() {
        use crate::normalize::{NormalizeOptions, Styling};

        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/10_013s.wav");
        let (pcm, rate) = read_wav(&clip);

        let engine = Engine::new(ConfigBuilder::new().build()).expect("engine must start");
        let default = engine
            .transcribe(Samples::new(pcm.clone(), rate, 1))
            .expect("queued")
            .recv()
            .expect("must transcribe");
        let casual = engine
            .transcribe_with(
                Samples::new(pcm, rate, 1),
                NormalizeOptions { styling: Styling::Casual, ..NormalizeOptions::default() },
            )
            .expect("queued")
            .recv()
            .expect("must transcribe");

        let (Outcome::Transcript(a), Outcome::Transcript(b)) = (default, casual) else {
            panic!("both runs must produce transcripts");
        };
        assert_eq!(a.asr, b.asr, "the same audio must recognise identically; only styling differs");
        assert_ne!(
            a.text, b.text,
            "a styling override that changes nothing is a decorative argument: {}",
            a.text
        );
    }

    /// Diagnostics must never break a take: a store directory that
    /// cannot be created is logged and dropped, and the words land
    /// anyway.
    #[test]
    #[ignore = "needs the ASR weights; run with --run-ignored all after `--example fetch`"]
    fn a_failed_diagnostics_store_does_not_fail_the_take() {
        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/08_009s.wav");
        let (pcm, rate) = read_wav(&clip);
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "a regular file, so nothing can be created under it").unwrap();

        let cfg =
            ConfigBuilder::new().normalizer(None).diagnostics_dir(blocker.join("store")).build();
        let engine = Engine::new(cfg).expect("engine must start");
        let outcome = engine
            .transcribe(Samples::new(pcm, rate, 1))
            .expect("queued")
            .recv()
            .expect("recognition must succeed");
        assert!(
            matches!(outcome, Outcome::Transcript(_)),
            "the take must land over a dead store, got {outcome:?}"
        );
    }

    /// The wiring leg: a real take populates the store, not just the
    /// store functions in isolation. The artifacts must agree with the
    /// answer the caller got - joined.txt is that take's exact
    /// normalizer input, and the capture is that take's own audio.
    #[test]
    #[ignore = "needs the ASR weights; run with --run-ignored all after `--example fetch`"]
    fn a_live_take_populates_the_diagnostics_store() {
        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/08_009s.wav");
        let (pcm, rate) = read_wav(&clip);
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigBuilder::new().normalizer(None).diagnostics_dir(dir.path()).build();
        let engine = Engine::new(cfg).expect("engine must start");
        let outcome = engine
            .transcribe(Samples::new(pcm.clone(), rate, 1))
            .expect("queued")
            .recv()
            .expect("recognition must succeed");
        let Outcome::Transcript(transcript) = outcome else {
            panic!("a spoken take must not read as silence: {outcome:?}");
        };
        // The store entry is written after the reply, by design; joining
        // the worker is what makes the write observable here.
        drop(engine);

        let entries: Vec<_> = std::fs::read_dir(dir.path()).expect("store dir").flatten().collect();
        assert_eq!(entries.len(), 1, "one take makes one store entry");
        let take = entries[0].path();

        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(take.join("meta.json")).unwrap())
                .expect("meta.json parses");
        assert_eq!(meta["outcome"], "transcript", "the take's own outcome is recorded");
        let duration_ms = u64::try_from(pcm.len()).unwrap_or(u64::MAX) * 1000 / 16_000;
        assert_eq!(
            meta["duration_ms"], duration_ms,
            "the metadata carries the capture's own length"
        );

        assert_eq!(
            std::fs::read_to_string(take.join("joined.txt")).unwrap(),
            transcript.asr,
            "joined.txt is that take's exact normalizer input"
        );
        assert_eq!(
            std::fs::read_to_string(take.join("text.txt")).unwrap(),
            transcript.text,
            "text.txt is that take's final text"
        );

        let mut reader = hound::WavReader::open(take.join("output.wav")).unwrap();
        assert_eq!(
            reader.spec().sample_rate,
            SAMPLE_RATE,
            "the capture is stored at the model rate"
        );
        assert_eq!(reader.samples::<i16>().count(), pcm.len(), "the whole capture is stored");
    }

    /// The `Config` -> `Engine` settings leg, which is a DIFFERENT path
    /// from the per-call override above.
    ///
    /// `a_styling_override_changes_the_text` goes through
    /// `transcribe_with`; this one sets the styling on the config and
    /// calls plain `transcribe`, so it is the only thing that catches
    /// `Engine::new` dropping `cfg.normalize_options` on the floor.
    #[test]
    #[ignore = "needs both models; run with --run-ignored all after `--example fetch`"]
    fn config_styling_reaches_the_model_without_a_per_call_override() {
        use crate::normalize::{NormalizeOptions, Styling};

        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/10_013s.wav");
        let (pcm, rate) = read_wav(&clip);

        let shipped = Engine::new(ConfigBuilder::new().build()).expect("engine must start");
        let casual = Engine::new(
            ConfigBuilder::new()
                .normalize_options(NormalizeOptions {
                    styling: Styling::Casual,
                    ..NormalizeOptions::default()
                })
                .build(),
        )
        .expect("engine must start");

        let a = shipped
            .transcribe(Samples::new(pcm.clone(), rate, 1))
            .expect("queued")
            .recv()
            .expect("must transcribe");
        let b = casual
            .transcribe(Samples::new(pcm, rate, 1))
            .expect("queued")
            .recv()
            .expect("must transcribe");

        let (Outcome::Transcript(a), Outcome::Transcript(b)) = (a, b) else {
            panic!("both runs must produce transcripts");
        };
        assert_eq!(a.asr, b.asr, "the same audio must recognise identically; only styling differs");
        assert_ne!(
            a.text, b.text,
            "config styling that never reaches the model is a setting in name only: {}",
            a.text
        );
    }

    /// Teardown must abandon queued work rather than run it.
    ///
    /// Counts jobs rather than timing the drop, so machine load cannot
    /// flake it and a fast machine cannot make it vacuous.
    #[test]
    #[ignore = "needs the ASR weights; run with --run-ignored all after `--example fetch`"]
    fn dropping_the_engine_discards_the_backlog_instead_of_draining_it() {
        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/08_009s.wav");
        assert!(clip.exists(), "fixture missing at {}", clip.display());
        let (pcm, rate) = read_wav(&clip);

        let engine =
            Engine::new(ConfigBuilder::new().normalizer(None).build()).expect("engine must start");
        let queued: Vec<_> = (0..6)
            .map(|_| {
                engine
                    .transcribe(Samples::new(pcm.clone(), rate, 1))
                    .expect("a valid source queues")
            })
            .collect();

        // Joins the worker, so every job has resolved by the time this
        // returns and each `recv` below answers immediately.
        drop(engine);

        let answers: Vec<_> = queued.into_iter().map(Ticket::recv).collect();
        // Weights load on the worker, so `Engine::new` succeeds with no
        // model on disk - and then every job fails, every `ok()` is None,
        // and the count below reads zero for the wrong reason.
        let unloadable =
            answers.iter().filter(|a| matches!(a, Err(Error::ModelLoad { .. }))).count();
        assert_eq!(unloadable, 0, "the weights must actually be present, or this proves nothing");

        let completed = answers.iter().filter(|a| a.is_ok()).count();
        assert!(
            completed <= 1,
            "teardown must discard the backlog: at most the in-flight job may finish, {completed} of 6 did"
        );
    }
}
