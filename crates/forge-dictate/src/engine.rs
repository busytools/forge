//! The transcription engine and the tickets it hands back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
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

/// How far a take's transcription has got, for a host that shows
/// progress while the recording runs. `done` counts the segments that
/// have settled, in order; `total` is `None` until finish has
/// submitted the tail, because a live recording does not know how many
/// segments it will produce. A single-segment take reports twice at
/// stop, at 0 of 1 and 1 of 1, and no host has to special-case it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowProgress {
    pub done: usize,
    pub total: Option<usize>,
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
    /// This is what fitted rather than what was said: the recording hit
    /// [`Config::max_capture`], the decode ran out of its output budget,
    /// a window stopped on a repetition loop, or a window failed after
    /// earlier windows produced words. Carried here rather than only
    /// logged: a host cannot tell from a log line it may not be routing
    /// that part of the take is missing.
    ///
    /// Which cause it was depends on the path. Audio that arrived through
    /// [`Engine::transcribe`] or [`Engine::transcribe_with`] was never
    /// capped, so a cap is not among its causes; on the capture path a
    /// host can compare [`Stages::audio`] against its configured cap to
    /// tell the cap from the rest.
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
            tracing::warn!(
                event_name = "normalization_failed",
                %error,
                "normalization failed; returning raw text"
            );
            asr.to_owned()
        }
    }
}

/// Samples to wall-clock at the one rate the models accept. Integer
/// maths so a long buffer cannot drift through a float cast.
fn audio_duration(samples: usize) -> Duration {
    Duration::from_micros((samples as u64).saturating_mul(1_000_000) / u64::from(SAMPLE_RATE))
}

/// Loudest sample in `pcm`, in dBFS.
fn peak_dbfs(pcm: &[f32]) -> f32 {
    let peak = pcm.iter().fold(0.0f32, |worst, s| worst.max(s.abs()));
    if peak <= 0.0 { f32::NEG_INFINITY } else { 20.0 * peak.log10() }
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
    while let Some(cut) = next_cut(pcm, start) {
        bounds.push((start, cut));
        start = cut;
    }
    bounds.push((start, pcm.len()));
    bounds
}

/// Where the next window past `start` ends, once more than
/// [`WINDOW_TARGET`] has accrued past it: the quietest [`ENERGY_FRAME`]
/// block between [`WINDOW_MIN`] and [`WINDOW_TARGET`] past `start`.
/// `None` while the region is still short.
///
/// This one function is the whole windowing rule, and both callers
/// share it: whole-buffer windowing loops it over the finished take,
/// and the live segmenter calls it as the recording grows. The cut at
/// `start` depends only on samples up to `start + WINDOW_TARGET`, so
/// deciding it as soon as they exist lands exactly where deciding it
/// after the take would.
fn next_cut(pcm: &[f32], start: usize) -> Option<usize> {
    if pcm.len() - start <= WINDOW_TARGET {
        return None;
    }
    let lo = start + WINDOW_MIN;
    let hi = start + WINDOW_TARGET;
    Some(lo + quietest_frame(&pcm[lo..hi]))
}

/// Sample offset of the quietest [`ENERGY_FRAME`] block by summed
/// square. Ties go to the latest block, so a window runs as long as the
/// reliability ceiling allows and a flat pause cuts at its end. Zero
/// for a region shorter than one block.
fn quietest_frame(pcm: &[f32]) -> usize {
    let mut quietest = usize::MAX;
    let mut quietest_energy = f32::INFINITY;
    for (frame, block) in pcm.as_chunks::<ENERGY_FRAME>().0.iter().enumerate() {
        let energy: f32 = block.iter().map(|s| s * s).sum();
        if energy <= quietest_energy {
            quietest_energy = energy;
            quietest = frame;
        }
    }
    if quietest == usize::MAX { 0 } else { quietest * ENERGY_FRAME }
}

/// The store entry a job's answer earns: `None` keeps nothing.
///
/// An abandoned take (the user's own cancel) and a take the silence
/// gate resolved before the weights persist nothing - there is no
/// transcript stage to examine and nothing was recognised. Everything
/// else keeps its audio: an `empty` or a `recognition_error` are
/// exactly the takes worth opening files over.
fn diagnostic_outcome(answer: &Result<Outcome, Error>) -> Option<(&'static str, String)> {
    match answer {
        Err(Error::Cancelled) | Ok(Outcome::NoAudio { .. }) => None,
        Err(_) => Some(("recognition_error", String::new())),
        Ok(Outcome::Transcript(transcript)) => {
            let label = if transcript.text.is_empty() { "empty" } else { "transcript" };
            Some((label, transcript.text.clone()))
        }
    }
}

/// The store entry a finished answer earns, read with the cancel token at
/// the write decision.
///
/// The early checks catch a cancel that landed before the answer was built,
/// and [`finish_after_join`] catches one that landed during the join's
/// normalize pass. Neither can cover the write itself: both writers read the
/// token here first, and [`diagnostics::capture_take`] removes the directory
/// it claimed when the token is set by the time the write finishes. Together
/// that is what makes an abandoned take persist nothing.
fn kept_diagnostics(
    cancel: &CancelToken,
    answer: &Result<Outcome, Error>,
) -> Option<(&'static str, String)> {
    if cancel.is_cancelled() { None } else { diagnostic_outcome(answer) }
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
    /// `Some` on a job that rewrites its own text: the direct path,
    /// and the pipelined finalize. Pipelined segment jobs carry `None`
    /// so their raw text joins later under one normalize pass.
    normalize: Option<NormalizeOptions>,
    /// Set on a pipelined finalize job: rewrite this joined text
    /// instead of recognizing `pcm`, which is empty.
    prejoined: Option<String>,
    /// `false` on pipelined jobs: one take writes ONE diagnostics
    /// record, from its segmenter at finish, not one per segment.
    diagnose: bool,
    cancel: CancelToken,
    reply: Sender<Result<Outcome, Error>>,
    queued_at: Instant,
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
    normalize_options: NormalizeOptions,
    silence_floor: f32,
    /// Where take diagnostics land, if the caller pointed them
    /// anywhere. Held here because the worker consumes the config, and
    /// the per-take segmenter writes the record at finish.
    diagnostics_dir: Option<PathBuf>,
    /// The models this engine runs, resolved from the config before the
    /// worker consumes it: what every take's diagnostics record names
    /// itself by.
    models: Arc<diagnostics::TakeModels>,
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
        let normalize_options = cfg.normalize_options;
        let silence_floor = cfg.silence_floor;
        let diagnostics_dir = cfg.diagnostics_dir.clone();
        let models = Arc::new(diagnostics::TakeModels::from_config(&cfg));
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
                let models = Arc::clone(&models);
                move || worker(&asr_path, &cfg, &models, &queue, &stopping, &in_flight, &readiness)
            })
            .map_err(|source| Error::WorkerSpawn { message: source.to_string() })?;

        Ok(Arc::new(Engine {
            max_capture,
            normalize_options,
            silence_floor,
            diagnostics_dir,
            models,
            stopping,
            in_flight,
            jobs: Some(jobs),
            worker: Some(handle),
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

    /// Start a take fed by pushed frames.
    ///
    /// The one refusal is an engine that is winding down, whose drain
    /// would discard every job the take queued.
    pub fn capture_stream(self: &Arc<Self>) -> Result<StreamCapture, Error> {
        if self.stopping.load(Ordering::Relaxed) {
            return Err(Error::EngineStopped);
        }
        let jobs = self.jobs.as_ref().ok_or(Error::EngineStopped)?.clone();
        let parts = self.begin_take(jobs);
        let Some(segmenter) = parts.segmenter else {
            return Err(Error::Capture { message: "the take segmenter did not start".into() });
        };
        Ok(StreamCapture {
            sink: FrameSink {
                recording: Arc::clone(&parts.recording),
                limit: crate::capture::sample_cap(self.max_capture),
            },
            capture: Capture {
                engine: Arc::clone(self),
                recording: parts.recording,
                max_capture: self.max_capture,
                segmenter: Some(segmenter),
                finish_tx: Some(parts.finish_tx),
                answer_rx: Some(parts.answer_rx),
                progress_rx: Some(parts.progress_rx),
                cancel: parts.cancel,
            },
        })
    }

    /// The take side: the recording `sample_cap`
    /// samples long, the segmenter cutting windows off it as it grows,
    /// and the channels a [`Capture`] hands on at `finish`.
    fn begin_take(self: &Arc<Self>, jobs: Sender<Job>) -> TakeParts {
        let recording =
            Arc::new(crate::capture::Recording::new(crate::capture::sample_cap(self.max_capture)));
        let (finish_tx, finish_rx) = channel::<FinishTake>();
        let (answer_tx, answer_rx) = channel();
        let (progress_tx, progress_rx) = channel();
        let cancel = CancelToken::new();
        let take = TakeShared { cancel: cancel.clone(), answer: answer_tx, progress: progress_tx };
        let segmenter = std::thread::Builder::new()
            .name("forge-dictate-take".into())
            .spawn({
                let state = TakeSegmenter {
                    recording: Arc::clone(&recording),
                    jobs,
                    silence_floor: self.silence_floor,
                    diagnostics_dir: self.diagnostics_dir.clone(),
                    models: Arc::clone(&self.models),
                    take,
                    last_cut: 0,
                    cuts: Vec::new(),
                    parts: Vec::new(),
                    pending: Vec::new(),
                    settled: 0,
                    first_queued_at: None,
                    first_settled_at: None,
                    failed: None,
                };
                move || state.run(&finish_rx)
            })
            .ok();
        TakeParts { recording, segmenter, finish_tx, answer_rx, progress_rx, cancel }
    }

    /// Queue already-captured audio.
    fn submit(
        &self,
        pcm: Vec<f32>,
        resample: Duration,
        truncated: bool,
        options: NormalizeOptions,
    ) -> Result<Ticket, Error> {
        let queued_at = Instant::now();
        let (reply, answer) = channel();
        let cancel = CancelToken::new();

        // Silence is a property of the samples, so it is decided here
        // rather than on the worker: a quiet capture needs no weights,
        // should not load any, and should not queue behind a backlog.
        let peak = peak_dbfs(&pcm);
        let audio = audio_duration(pcm.len());
        if peak < self.silence_floor {
            let _ = reply.send(Ok(Outcome::NoAudio { peak, audio }));
            return Ok(Ticket { answer, cancel, segmenter: None });
        }

        self.jobs
            .as_ref()
            .ok_or(Error::EngineStopped)?
            .send(Job {
                pcm,
                resample,
                audio,
                truncated,
                normalize: Some(options),
                prejoined: None,
                diagnose: true,
                cancel: cancel.clone(),
                reply,
                queued_at,
            })
            .map_err(|_| Error::EngineStopped)?;
        Ok(Ticket { answer, cancel, segmenter: None })
    }
}

/// A take fed by frames from somewhere other than a device: what
/// [`Engine::capture_stream`] hands back.
///
/// The sink is what a transport holds and pushes into; the capture is
/// what the host finishing the take holds. Both share one recording, so
/// a take fed this way meters, caps, segments, cancels and finishes on
/// one path.
pub struct StreamCapture {
    pub sink: FrameSink,
    pub capture: Capture,
}

/// The write half of a stream-fed take.
///
/// `Send` and `Sync`, so a socket task or any other reader of frames can
/// hold it and push from wherever it runs.
pub struct FrameSink {
    recording: Arc<crate::capture::Recording>,
    limit: usize,
}

impl FrameSink {
    /// Push mono samples at [`SAMPLE_RATE`].
    ///
    /// The loudest sample is folded into the meter, so a level bar reads
    /// pushed audio. Samples past [`Config::max_capture`] are dropped and
    /// the take flags itself truncated. Answers `false` once the take is
    /// over, so a caller can stop feeding.
    pub fn push_mono(&self, samples: &[f32]) -> bool {
        if self.recording.stopped() {
            return false;
        }
        self.recording.push(samples, self.limit);
        !self.recording.stopped()
    }
}

impl std::fmt::Debug for FrameSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameSink").field("limit", &self.limit).finish_non_exhaustive()
    }
}

/// The polling view of a live capture: the level read and the cap
/// flag, over a shared handle to the recording. `Sync` and `Send`, so
/// an async host can hold it across await points.
pub struct CaptureMeter {
    recording: Arc<crate::capture::Recording>,
}

impl CaptureMeter {
    /// Loudest input since the last read, in dBFS. Take-and-reset, as
    /// [`Capture::level`] - the handle shares the recording's
    /// accumulator, so exactly one poller should read it.
    pub fn level(&self) -> f32 {
        self.recording.peak_dbfs()
    }

    /// Whether the capture reached [`Config::max_capture`] and stopped
    /// itself.
    pub fn was_truncated(&self) -> bool {
        self.recording.was_truncated()
    }

    /// Stop the take at its cap, flagging it truncated. For a take fed by
    /// frames, whose host counts the clock: nothing else notices the cap
    /// when the audio stops arriving.
    pub fn cap_reached(&self) {
        self.recording.cap_reached();
    }
}

impl std::fmt::Debug for CaptureMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureMeter").finish_non_exhaustive()
    }
}

/// One take in flight: the recording it grows, its segmenter, and the
/// channels `finish` hands on.
///
/// Release is tied to the value rather than to a method, so a caller
/// that panics mid-take cannot leave the take's threads running.
pub struct Capture {
    engine: Arc<Engine>,
    recording: Arc<crate::capture::Recording>,
    max_capture: Duration,
    /// The take's segmenter. Taken by `finish`, which hands it to the
    /// ticket; joined by `Drop` on the abandon path.
    segmenter: Option<std::thread::JoinHandle<()>>,
    /// Dropping it aborts the segmenter: the take is over and its
    /// queued work should not run on.
    finish_tx: Option<Sender<FinishTake>>,
    /// The take's aggregate answer, once every segment and the tail
    /// have run and the join has been normalized. Taken by `finish`.
    answer_rx: Option<Receiver<Result<Outcome, Error>>>,
    /// Per-segment progress while the recording is still open. Taken
    /// by the host at recording start; the stream outlives the handoff
    /// to the ticket because the segmenter owns its sender throughout.
    progress_rx: Option<Receiver<WindowProgress>>,
    /// This take's cancel token, shared by every job the segmenter
    /// queues. The ticket carries a clone, so abandoning the take
    /// after finish aborts its outstanding work.
    cancel: CancelToken,
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

    /// The shareable polling view of this capture: level and cap
    /// truncation, the two reads a recording loop wants. `Capture`
    /// itself is `Send` but not `Sync` - it holds channel receivers -
    /// so a host that polls a live capture from an async task borrows
    /// this handle across awaits instead of the capture.
    pub fn meter(&self) -> CaptureMeter {
        CaptureMeter { recording: Arc::clone(&self.recording) }
    }

    /// Take this take's progress stream, if it has not been taken.
    /// Steps arrive as segments settle while the recording is still
    /// open, and again once finish has submitted the tail and the
    /// total is known.
    pub fn take_progress(&mut self) -> Option<Receiver<WindowProgress>> {
        self.progress_rx.take()
    }

    /// Stop the take and queue what was captured.
    pub fn finish(self) -> Result<Ticket, Error> {
        let options = self.engine.normalize_options;
        self.finish_with(options)
    }

    /// Stop recording and queue what was captured, overriding the
    /// configured normalizer options for this recording only.
    ///
    /// With a take already pipelining, this submits the tail and hands
    /// back a ticket over the AGGREGATE: the segmenter waits for every
    /// segment already queued, joins the raw texts, and normalizes the
    /// join once on the worker. The returned ticket resolves when the
    /// whole transcript does.
    pub fn finish_with(mut self, options: NormalizeOptions) -> Result<Ticket, Error> {
        self.stop_recording();
        let truncated = self.recording.was_truncated();
        if truncated {
            tracing::debug!(
                event_name = "capture_cap_reached",
                cap = ?self.max_capture,
                "capture reached its cap and stopped itself"
            );
        }
        let segmenter = self.segmenter.take();
        match (self.finish_tx.take(), self.answer_rx.take(), segmenter) {
            (Some(finish_tx), Some(answer_rx), Some(segmenter)) => {
                finish_tx
                    .send(FinishTake { options, truncated })
                    .map_err(|_| Error::EngineStopped)?;
                Ok(Ticket {
                    answer: answer_rx,
                    cancel: self.cancel.clone(),
                    segmenter: Some(segmenter),
                })
            }
            _ => Err(Error::EngineStopped),
        }
    }

    /// Stop recording and throw the audio away.
    pub fn cancel(self) {
        drop(self);
    }

    /// The cap this take is bounded by. A host feeding the take itself has
    /// to enforce it: a take fed by frames has no loop at all.
    pub fn max_capture(&self) -> Duration {
        self.max_capture
    }

    /// Whether the take reached [`Config::max_capture`] and stopped
    /// itself. A host polling the level reads this so it can submit the
    /// take instead of holding a seat whose take is no longer running.
    pub fn was_truncated(&self) -> bool {
        self.recording.was_truncated()
    }

    /// Stop the take: the recording's stop flag is what tells a pusher
    /// the take is over. The captured samples stay where they are: the
    /// segmenter holds the recording and reads the tail out of it at
    /// finish.
    fn stop_recording(&mut self) {
        self.recording.stop();
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Stop first, then the segmenter: the closed finish channel is
        // the abandon signal, and the join makes sure no take thread
        // outlives the value that owns its result.
        self.stop_recording();
        self.finish_tx.take();
        if let Some(segmenter) = self.segmenter.take() {
            let _ = segmenter.join();
        }
    }
}

/// A transcription in flight. On the pipelined capture path this is
/// the take's AGGREGATE: it resolves once every segment and the tail
/// have run and the join has been normalized once.
pub struct Ticket {
    answer: Receiver<Result<Outcome, Error>>,
    cancel: CancelToken,
    /// The take's segmenter, on the capture path. Joined so no take
    /// thread outlives the value that owns its result; it holds no
    /// native state, so the join is tidiness and not the teardown
    /// guarantee - that is the worker join in `Engine::drop`.
    segmenter: Option<std::thread::JoinHandle<()>>,
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
    pub fn recv(mut self) -> Result<Outcome, Error> {
        let answer = self.answer.recv().map_err(|_| Error::EngineStopped)?;
        if let Some(segmenter) = self.segmenter.take() {
            let _ = segmenter.join();
        }
        answer
    }

    /// A clone of this ticket's cancel token. Cancelling it aborts THIS
    /// transcription - a job still queued behind another takes the
    /// token with it and is aborted when its turn comes - which is why
    /// a host abandoning one take among several goes through here
    /// rather than through anything engine-wide.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(segmenter) = self.segmenter.take() {
            let _ = segmenter.join();
        }
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
        let declared = model.capabilities()?.native_sample_rate;
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

/// What one window's run outcome tells the take to do next.
///
/// Extracted from the window loop so the decisions can be tested without a
/// loaded model: the loop owns the timings, the records and the reply, while
/// this owns what each outcome means.
#[derive(Debug, PartialEq)]
enum WindowAction {
    /// Append this text and keep going.
    Continue(String),
    /// Append this partial text and keep going; the take is truncated.
    Truncated(String),
    /// Stop the take: the user cancelled.
    Cancelled,
    /// Stop the take: this window failed, but earlier windows produced words.
    /// The take finalizes what it has, marks itself truncated, and carries
    /// the error into diagnostics.
    FinalizeWithError { message: String },
    /// Stop the take: this window failed and nothing was transcribed, so the
    /// take fails.
    Failed { message: String },
}

fn window_action(
    result: Result<transcribe_cpp::Transcript, transcribe_cpp::Error>,
    has_prior_text: bool,
) -> WindowAction {
    match result {
        Ok(out) => WindowAction::Continue(out.text),
        Err(transcribe_cpp::Error::Aborted { .. }) => WindowAction::Cancelled,
        Err(
            transcribe_cpp::Error::OutputTruncated { partial: Some(partial), .. }
            | transcribe_cpp::Error::OutputRepetition { partial: Some(partial), .. },
        ) => WindowAction::Truncated(partial.text),
        Err(source) => {
            let message = source.to_string();
            if has_prior_text {
                WindowAction::FinalizeWithError { message }
            } else {
                WindowAction::Failed { message }
            }
        }
    }
}

/// The failure a job answers with, decided before its answer is built.
///
/// `session.run` is the only place a cancel token is observed, so a take
/// cancelled after its last window has no run left to abort: without this
/// check the worker builds a transcript for a take its caller abandoned, and
/// a host persists words the user chose not to keep. A cancel outranks
/// whatever failure the fold already carries, the same rule the take fold
/// applies. Pulled out of the worker so the decision is tested without a
/// loaded model, the way [`window_action`] is.
fn failure_before_answer(cancel: &CancelToken, failure: Option<Error>) -> Option<Error> {
    if cancel.is_cancelled() { Some(Error::Cancelled) } else { failure }
}

/// What one job's answer and diagnostics record come to.
struct JobOutcome {
    answer: Result<Outcome, Error>,
    /// The stages the record is built from, with the normalize pass's own
    /// timing already folded in.
    stages: Stages,
    /// The record this answer earns, read with the job's own token.
    diag: Option<(&'static str, String)>,
}

/// Finish one job: read its token, build the answer, decide its record.
///
/// Taking `&Job` rather than a bare token is what keeps the wiring honest:
/// every read in here is the job's own, so the worker cannot hand the
/// answer one token and the record another. Drivable by a test over a
/// prejoined job without a loaded model.
fn finish_job(
    job: &Job,
    asr_parts: &[String],
    failure: Option<Error>,
    truncated: bool,
    mut stages: Stages,
    normalizer: Option<&crate::normalize::Normalizer>,
) -> JobOutcome {
    // `session.run` is the only place the recognizer observes a cancel, so
    // a take cancelled after its last window has no run left to abort:
    // read the token before anything is built.
    let failure = failure_before_answer(&job.cancel, failure);
    if let Some(error) = failure {
        let answer = Err(error);
        let diag = kept_diagnostics(&job.cancel, &answer);
        return JobOutcome { answer, stages, diag };
    }
    let (asr, text) = finalize_transcript(asr_parts, |raw| match job.normalize {
        // A normalizer that fails mid-session must not cost the speaker
        // their words: fall back to the recognised text and say so, where a
        // load failure above is fatal. A raw-only pipelined job skips the
        // rewrite; its text joins the take's later, under one normalize
        // pass.
        Some(options) => normalize_text(normalizer, raw, options, &mut stages),
        None => raw.to_owned(),
    });
    finish_after_join(job, asr, text, stages, truncated)
}

/// The answer a finished join amounts to.
///
/// Separated from [`finish_job`] because the join's normalize pass is the
/// one stretch no early read covers: a test enters here with a cancelled
/// token to model a cancel that landed inside it, and the read below is
/// what turns that into `Cancelled` rather than the words the pass rewrote.
fn finish_after_join(
    job: &Job,
    asr: String,
    text: String,
    stages: Stages,
    truncated: bool,
) -> JobOutcome {
    let answer = if let Some(error) = failure_before_answer(&job.cancel, None) {
        Err(error)
    } else {
        let transcript = Transcript { text, asr, stages: stages.clone(), truncated };
        Ok(Outcome::Transcript(transcript))
    };
    let diag = kept_diagnostics(&job.cancel, &answer);
    JobOutcome { answer, stages, diag }
}

/// The take's accumulated state across its windows.
///
/// Extracted from the loop so the side effects of each outcome can be
/// tested: a `FinalizeWithError` keeps the take alive, a `Failed` fails it.
#[derive(Default)]
struct TakeState {
    asr_parts: Vec<String>,
    window_records: Vec<diagnostics::WindowRecord>,
    truncated: bool,
    failure: Option<Error>,
    recognition_error: Option<(usize, String)>,
}

impl TakeState {
    fn new(truncated: bool) -> Self {
        Self { truncated, ..Self::default() }
    }

    /// Whether any earlier window produced words.
    fn has_prior_text(&self) -> bool {
        self.asr_parts.iter().any(|part| !part.trim().is_empty())
    }

    /// Apply one window's outcome. Answers `false` when the loop should stop.
    fn apply(&mut self, action: WindowAction, index: usize, start_ms: u64, end_ms: u64) -> bool {
        match action {
            WindowAction::Continue(text) => {
                self.window_records.push(diagnostics::WindowRecord {
                    start_ms,
                    end_ms,
                    raw: text.clone(),
                });
                self.asr_parts.push(text);
                true
            }
            WindowAction::Truncated(text) => {
                self.window_records.push(diagnostics::WindowRecord {
                    start_ms,
                    end_ms,
                    raw: text.clone(),
                });
                self.asr_parts.push(text);
                self.truncated = true;
                true
            }
            WindowAction::Cancelled => {
                self.failure = Some(Error::Cancelled);
                false
            }
            WindowAction::FinalizeWithError { message } => {
                self.truncated = true;
                self.recognition_error = Some((index, message));
                false
            }
            WindowAction::Failed { message } => {
                self.failure = Some(Error::Recognition { message: message.clone() });
                self.recognition_error = Some((index, message));
                false
            }
        }
    }
}

/// Owns the weights and drains the queue.
fn worker(
    asr_path: &Path,
    cfg: &Config,
    models: &diagnostics::TakeModels,
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
            event_name = "model_cancellation_unsupported",
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
        let mut state = TakeState::new(job.truncated);
        let started = Instant::now();
        // How long the take sat before its first window ran: the queue,
        // and for early takes the tail of the model load. The felt lag
        // `processing_ms` never sees.
        let start_lag_ms = u64::try_from(job.queued_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let window_ms =
            |samples: usize| u64::try_from(audio_duration(samples).as_millis()).unwrap_or(u64::MAX);
        // A pipelined finalize job carries text, not audio: it exists
        // so the join is normalized on the worker, where the normalizer
        // lives. Everything else recognizes its windows.
        // Cloned rather than moved so `finish_job` can still read the job's
        // own token and settings; a join is one string against a take.
        if let Some(raw) = job.prejoined.clone() {
            state.asr_parts.push(raw);
        } else {
            let windows = window_bounds(&job.pcm);
            for (k, &(start, end)) in windows.iter().enumerate() {
                let result = session.run(&job.pcm[start..end], &options);
                if let Ok(out) = &result {
                    stages.mel = stages.mel.saturating_add(Duration::from_secs_f64(
                        f64::from(out.timings.mel_ms) / 1000.0,
                    ));
                    stages.encode = stages.encode.saturating_add(Duration::from_secs_f64(
                        f64::from(out.timings.encode_ms) / 1000.0,
                    ));
                    stages.decode = stages.decode.saturating_add(Duration::from_secs_f64(
                        f64::from(out.timings.decode_ms) / 1000.0,
                    ));
                }
                // Discriminated on the ERROR VARIANT rather than on
                // `was_aborted`/`was_truncated`. Those report "the most
                // recent run", and `run` has early returns that never reach
                // native at all - an interior NUL in the language string, an
                // oversized buffer, a busy session - on which the flags still
                // hold the PREVIOUS job's value. Per-error state cannot go
                // stale.
                let action = window_action(result, state.has_prior_text());
                if !state.apply(action, k, window_ms(start), window_ms(end)) {
                    break;
                }
            }
        }
        let TakeState { asr_parts, window_records, truncated, failure, recognition_error } = state;
        // The diagnostics record's copy of the stages is taken beside the
        // answer rather than before it: the cleanup pass sets
        // `Stages::normalize` on the way, so a clone made first would
        // report `normalizer_ran` false for a take that ran it.
        let JobOutcome { answer, stages: diag_stages, diag } =
            finish_job(&job, &asr_parts, failure, truncated, stages, normalizer.as_ref());
        // Consumed on a transcript rather than where `stages` is built: an
        // error or a cancel discards the stages, and taking the load cost
        // there would lose it for the process.
        if matches!(&answer, Ok(Outcome::Transcript(_))) {
            first = false;
        }
        in_flight.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        let _ = job.reply.send(answer);

        // Best-effort diagnostics, after the take has landed: the reply
        // is never held back by a write. An abandoned take persists
        // nothing (the classifier's None). Teardown skips the write:
        // quitting fast beats blocking shutdown on a capture-sized
        // write, and that is a stated trade, not a side effect. A
        // queued take starts after the write, which is the one cost a
        // 30-minute capture's wav ever asks anyone to pay. Pipelined
        // jobs never write at all - the take writes ONE record, from
        // its segmenter at finish.
        if let (Some(dir), Some((outcome, text))) = (&cfg.diagnostics_dir, diag)
            && job.diagnose
            && !stopping.load(Ordering::Relaxed)
        {
            let record = diagnostics::TakeRecord {
                audio: &job.pcm,
                windows: &window_records,
                joined: &join_window_texts(&asr_parts),
                text: &text,
                stages: &diag_stages,
                models,
                start_lag_ms,
                processing_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                truncated,
                outcome,
                recognition_error,
            };
            diagnostics::capture_take(dir, diagnostics::take_stamp(), &record, &job.cancel);
        }
    }
}

/// How often the segmenter looks for the next window boundary. The cut
/// needs no samples past `WINDOW_TARGET` after the previous cut, so
/// this cadence bounds how long a segment can run past its ideal cut.
const SEGMENT_POLL: Duration = Duration::from_millis(250);

/// What the host hands a pipelined take at finish: the normalizer
/// options for the one join pass, and whether the recording stopped
/// itself at the cap.
#[derive(Clone, Copy)]
struct FinishTake {
    options: NormalizeOptions,
    truncated: bool,
}

/// The take side both capture paths share, from [`Engine::begin_take`]:
/// the recording samples land in, the segmenter cutting it, and the
/// channels a capture hands on at finish.
struct TakeParts {
    recording: Arc<crate::capture::Recording>,
    segmenter: Option<std::thread::JoinHandle<()>>,
    finish_tx: Sender<FinishTake>,
    answer_rx: Receiver<Result<Outcome, Error>>,
    progress_rx: Receiver<WindowProgress>,
    cancel: CancelToken,
}

/// The channels a take's segmenter shares with its capture and ticket.
struct TakeShared {
    /// Cancels every job the segmenter queued, when the take is over.
    cancel: CancelToken,
    /// The take's aggregate answer.
    answer: Sender<Result<Outcome, Error>>,
    /// Per-segment progress.
    progress: Sender<WindowProgress>,
}

/// One settled piece of a pipelined take, in submission order.
#[derive(Debug)]
enum Part {
    /// Recognition output for the join, with the stages it cost. The
    /// text is raw: normalization happens once, over the join.
    Transcript {
        raw: String,
        stages: Stages,
        truncated: bool,
    },
    /// Below the silence floor; contributes its span and peak only.
    NoAudio {
        peak: f32,
        audio: Duration,
    },
    Failed(Error),
}

/// What a take's settled parts come to, before normalization.
#[derive(Debug)]
enum Aggregate {
    /// Raw texts to join and normalize once. `failed` is set when a segment
    /// failed after the take already had words: the take keeps what it
    /// transcribed and marks itself truncated.
    Join {
        raws: Vec<String>,
        failed: Option<Error>,
    },
    NoAudio {
        peak: f32,
        audio: Duration,
    },
    Failed(Error),
}

/// Sum of the parts' recognition work, for the take's own stages and
/// the diagnostics record.
fn fold_stages(parts: &[Part]) -> (Stages, Duration, bool) {
    let mut stages = Stages::default();
    let mut audio = Duration::ZERO;
    let mut truncated = false;
    for part in parts {
        let Part::Transcript { stages: s, truncated: t, .. } = part else { continue };
        stages.mel += s.mel;
        stages.encode += s.encode;
        stages.decode += s.decode;
        stages.model_load += s.model_load;
        stages.resample += s.resample;
        if s.normalize.is_some() {
            stages.normalize = s.normalize;
        }
        truncated |= *t;
        audio += s.audio;
    }
    (stages, audio, truncated)
}

/// Whether a take that kept some words is still incomplete: a segment's own
/// truncation, the capture cap, or a failed segment.
fn take_is_truncated(folded_truncated: bool, finish_truncated: bool, failed: bool) -> bool {
    folded_truncated || finish_truncated || failed
}

/// Fold a take's parts in submission order. A failure fails the take only
/// when no part produced words; when any part did, the take keeps them and
/// carries the first failure to the caller, which marks the transcript
/// truncated. A cancel is the one exception: an abandoned take keeps
/// nothing. Any recognized part sends the take down the join path, and only
/// a take with nothing above the floor anywhere reads as silence.
fn aggregate_parts(parts: Vec<Part>) -> Aggregate {
    let mut raws = Vec::new();
    let mut peak = f32::NEG_INFINITY;
    let mut audio = Duration::ZERO;
    let mut failed = None;
    let mut cancelled = false;
    let mut saw_transcript = false;
    for part in parts {
        match part {
            Part::Transcript { raw, .. } => {
                saw_transcript = true;
                raws.push(raw);
            }
            Part::NoAudio { peak: p, audio: a } => {
                peak = peak.max(p);
                audio += a;
            }
            Part::Failed(error) => {
                if matches!(&error, Error::Cancelled) {
                    cancelled = true;
                }
                if failed.is_none() {
                    failed = Some(error);
                }
            }
        }
    }
    if cancelled {
        // A cancel anywhere in the take outranks every other failure: the
        // user abandoned it, and an abandoned take persists nothing.
        return Aggregate::Failed(Error::Cancelled);
    }
    let has_text = raws.iter().any(|raw| !raw.trim().is_empty());
    if let Some(error) = failed {
        // Any other failure keeps the words already transcribed, including
        // a stopped engine: the person did not ask to abandon the take.
        if !has_text {
            return Aggregate::Failed(error);
        }
        return Aggregate::Join { raws, failed: Some(error) };
    }
    if !saw_transcript {
        return Aggregate::NoAudio { peak, audio };
    }
    Aggregate::Join { raws, failed: None }
}

/// One capture's segmenter: cuts windows off the growing recording as
/// they pass the reliability ceiling and queues each for recognition
/// while the microphone keeps running; at finish it submits the tail,
/// waits out every segment, and normalizes the join exactly once, on
/// the worker where the normalizer lives. No partial text is ever
/// produced - the only thing that leaves this thread before the answer
/// is the progress count.
struct TakeSegmenter {
    recording: Arc<crate::capture::Recording>,
    jobs: Sender<Job>,
    silence_floor: f32,
    diagnostics_dir: Option<PathBuf>,
    /// The models the take ran under, named in its diagnostics record.
    models: Arc<diagnostics::TakeModels>,
    take: TakeShared,
    /// First sample not yet handed to a segment.
    last_cut: usize,
    /// Segment spans in submission order, for the diagnostics record.
    cuts: Vec<(usize, usize)>,
    /// Settled pieces, in submission order. Aligned with `cuts`: every
    /// cut contributes exactly one part, even a silence-gated one.
    parts: Vec<Part>,
    /// Answers still awaited, in submission order. The worker is
    /// serial, so they complete in this order too.
    pending: Vec<Receiver<Result<Outcome, Error>>>,
    /// How many parts have settled. The progress stream's `done`.
    settled: usize,
    /// When the take's first job was queued, and when its first part
    /// settled: the diagnostics `start_lag_ms` spans the two.
    first_queued_at: Option<Instant>,
    first_settled_at: Option<Instant>,
    /// The first recognition failure and the segment it hit, for the
    /// diagnostics record. The take answers with the same error.
    failed: Option<(usize, String)>,
}

impl TakeSegmenter {
    fn run(mut self, finish: &Receiver<FinishTake>) {
        loop {
            match finish.recv_timeout(SEGMENT_POLL) {
                Ok(finish) => {
                    self.finish(finish);
                    return;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Abandoned without finishing: the take is over,
                    // and its queued work should not run on.
                    self.take.cancel.cancel();
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.poll_cut();
                    self.collect_ready();
                }
            }
        }
    }

    /// Cut the next window once enough audio has accrued past the last
    /// one - the only poll that copies, since a boundary arrives every
    /// 30 to 60 seconds against a 250 ms poll, and the early return is
    /// exactly `next_cut`'s own short-circuit. The decision runs the
    /// same function whole-buffer windowing does, over a copy of the
    /// uncut region, so a live cut lands exactly where the finished
    /// take's would.
    fn poll_cut(&mut self) {
        let len = self.recording.sample_len();
        if len.saturating_sub(self.last_cut) <= WINDOW_TARGET {
            return;
        }
        let region = self.recording.copy_region(self.last_cut, len);
        let Some(relative) = next_cut(&region, 0) else { return };
        self.last_cut += relative;
        // `region[..relative]` is exactly the segment; the copy in front
        // of it doubles as the quietest-frame search space.
        let segment = region[..relative].to_vec();
        self.submit_region(segment);
    }

    /// Silence-gate a region and queue it, recording whichever part it
    /// became. `cuts` and `parts` advance together, so the diagnostics
    /// record can name every segment's span and raw text.
    fn submit_region(&mut self, pcm: Vec<f32>) {
        let start = self.cuts.last().map_or(0, |&(_, end)| end);
        let end = start + pcm.len();
        self.cuts.push((start, end));
        let peak = peak_dbfs(&pcm);
        let audio = audio_duration(pcm.len());
        if peak < self.silence_floor {
            self.parts.push(Part::NoAudio { peak, audio });
            self.stamp_settled();
            let _ = self.take.progress.send(WindowProgress { done: self.settled, total: None });
            return;
        }
        let queued_at = Instant::now();
        if self.first_queued_at.is_none() {
            self.first_queued_at = Some(queued_at);
        }
        let (reply, answer) = channel();
        let sent = self.jobs.send(Job {
            pcm,
            resample: Duration::ZERO,
            audio,
            truncated: false,
            normalize: None,
            prejoined: None,
            diagnose: false,
            cancel: self.take.cancel.clone(),
            reply,
            queued_at,
        });
        if sent.is_ok() {
            self.pending.push(answer);
        } else {
            // The engine is going away; the take fails with it.
            self.parts.push(Part::Failed(Error::EngineStopped));
            self.stamp_settled();
        }
    }

    /// Take every answer already waiting, in submission order.
    fn collect_ready(&mut self) {
        while !self.pending.is_empty() {
            let answer = match self.pending[0].try_recv() {
                Ok(answer) => answer,
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                // The worker died without replying; the take fails
                // with it rather than waiting forever.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(Error::EngineStopped),
            };
            self.pending.remove(0);
            self.record(answer);
            let _ = self.take.progress.send(WindowProgress { done: self.settled, total: None });
        }
    }

    /// Wait out every answer still owed, in submission order. Only
    /// finish calls this, and the worker answers every job it takes,
    /// so this terminates.
    fn drain_pending(&mut self) {
        while !self.pending.is_empty() {
            let answer = self.pending[0].recv().unwrap_or(Err(Error::EngineStopped));
            self.pending.remove(0);
            self.record(answer);
        }
    }

    fn record(&mut self, answer: Result<Outcome, Error>) {
        let part = match answer {
            Ok(Outcome::Transcript(t)) => {
                Part::Transcript { raw: t.asr, stages: t.stages, truncated: t.truncated }
            }
            Ok(Outcome::NoAudio { peak, audio }) => Part::NoAudio { peak, audio },
            Err(error) => {
                if self.failed.is_none() {
                    let index = self.parts.len();
                    self.failed = Some((index, error.to_string()));
                }
                Part::Failed(error)
            }
        };
        self.parts.push(part);
        self.stamp_settled();
    }

    fn stamp_settled(&mut self) {
        self.settled += 1;
        if self.first_settled_at.is_none() {
            self.first_settled_at = Some(Instant::now());
        }
    }

    /// Complete the take: wait out the remaining segments, submit the
    /// tail, and normalize the join once. The answer lands on the
    /// take's channel as its last act; the only earlier output is the
    /// progress tally gaining its total.
    fn finish(mut self, finish: FinishTake) {
        let started = Instant::now();
        self.drain_pending();
        // The tail: whatever grew past the last cut. The recording
        // stopped before the finish message was sent, so the span is
        // stable.
        let len = self.recording.sample_len();
        if len > self.last_cut {
            let pcm = self.recording.copy_region(self.last_cut, len);
            self.submit_region(pcm);
        }
        let total = self.cuts.len();
        let _ = self.take.progress.send(WindowProgress { done: self.settled, total: Some(total) });
        // The tail's raw text feeds the join, so it settles before the
        // normalize pass queues.
        self.drain_pending();
        // The raw text per segment, aligned with `self.cuts`; silence
        // and failure contribute nothing to the join.
        let raws: Vec<String> = self
            .parts
            .iter()
            .map(|part| match part {
                Part::Transcript { raw, .. } => raw.clone(),
                Part::NoAudio { .. } | Part::Failed(_) => String::new(),
            })
            .collect();
        let (folded_stages, audio, folded_truncated) = fold_stages(&self.parts);
        let aggregate = aggregate_parts(std::mem::take(&mut self.parts));
        let failed_before_join = matches!(&aggregate, Aggregate::Join { failed: Some(_), .. });
        // The final drain may have settled the last window, so no
        // `session.run` is left to observe a cancel. An abandoned take
        // answers cancelled here rather than queueing a join whose words it
        // would persist.
        let answer = if self.take.cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            match aggregate {
                Aggregate::Failed(error) => Err(error),
                Aggregate::NoAudio { peak, audio } => Ok(Outcome::NoAudio { peak, audio }),
                Aggregate::Join { raws: join_raws, failed } => {
                    let joined = join_window_texts(&join_raws);
                    match self.normalize_join(&joined, finish.options) {
                        Ok(Outcome::Transcript(mut transcript)) => {
                            // The take's stages: every segment's recognition
                            // work plus the finalize's normalize pass, over
                            // the span the take actually covers.
                            let normalize = transcript.stages.normalize;
                            transcript.stages = folded_stages.clone();
                            transcript.stages.normalize = normalize;
                            transcript.stages.audio = audio;
                            transcript.truncated |= take_is_truncated(
                                folded_truncated,
                                finish.truncated,
                                failed.is_some(),
                            );
                            Ok(Outcome::Transcript(transcript))
                        }
                        Ok(other) => Ok(other),
                        Err(error) => Err(error),
                    }
                }
            }
        };
        // Read at the write, not only before the join: a cancel that lands
        // during the normalize pass is seen by no earlier check.
        let diag = kept_diagnostics(&self.take.cancel, &answer);
        let diag_stages = match &answer {
            Ok(Outcome::Transcript(transcript)) => transcript.stages.clone(),
            _ => folded_stages,
        };
        // Teardown reports itself as a stopped engine, and skips its
        // write for the same trade the worker makes: quitting fast
        // beats blocking shutdown on a capture-sized write.
        let teardown = matches!(&answer, Err(Error::EngineStopped));
        let _ = self.take.answer.send(answer);
        let _ = self.take.progress.send(WindowProgress { done: total, total: Some(total) });
        if let (Some(dir), Some((outcome, text))) = (&self.diagnostics_dir, diag)
            && !teardown
        {
            let ms_of = |samples: usize| {
                u64::try_from(audio_duration(samples).as_millis()).unwrap_or(u64::MAX)
            };
            let windows: Vec<diagnostics::WindowRecord> = self
                .cuts
                .iter()
                .zip(raws.iter())
                .map(|((start, end), raw)| diagnostics::WindowRecord {
                    start_ms: ms_of(*start),
                    end_ms: ms_of(*end),
                    raw: raw.clone(),
                })
                .collect();
            let audio = self.recording.take();
            let start_lag_ms = match (self.first_queued_at, self.first_settled_at) {
                (Some(queued), Some(settled)) => {
                    u64::try_from(settled.saturating_duration_since(queued).as_millis())
                        .unwrap_or(u64::MAX)
                }
                _ => 0,
            };
            let record = diagnostics::TakeRecord {
                audio: &audio,
                windows: &windows,
                joined: &join_window_texts(&raws),
                text: &text,
                stages: &diag_stages,
                models: &self.models,
                start_lag_ms,
                processing_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                truncated: take_is_truncated(
                    folded_truncated,
                    finish.truncated,
                    failed_before_join,
                ),
                outcome,
                recognition_error: self.failed,
            };
            diagnostics::capture_take(dir, diagnostics::take_stamp(), &record, &self.take.cancel);
        }
    }

    /// Queue the join for normalization on the worker and wait it out.
    /// No silence gate: the job carries text, not audio, and the gate
    /// reads samples.
    fn normalize_join(&self, joined: &str, options: NormalizeOptions) -> Result<Outcome, Error> {
        let (reply, answer) = channel();
        self.jobs
            .send(Job {
                pcm: Vec::new(),
                resample: Duration::ZERO,
                audio: Duration::ZERO,
                truncated: false,
                normalize: Some(options),
                prejoined: Some(joined.to_owned()),
                diagnose: false,
                cancel: self.take.cancel.clone(),
                reply,
                queued_at: Instant::now(),
            })
            .map_err(|_| Error::EngineStopped)?;
        answer.recv().map_err(|_| Error::EngineStopped)?
    }
}

#[cfg(test)]
mod tests_engine {
    use super::*;
    use crate::{ConfigBuilder, ModelFacts, ModelSpec, Samples};

    /// An engine whose weights live in an empty directory. Enough for
    /// anything decided before a job is queued; a job would fail at
    /// `recv`, and these tests never queue one.
    fn engine_without_weights() -> (tempfile::TempDir, Arc<Engine>) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigBuilder::new().models_dir(dir.path()).normalizer(None).build();
        let engine = Engine::new(cfg).expect("engine must start");
        (dir, engine)
    }

    /// What the store keeps, decided weightless. The abandoned take
    /// persisting NOTHING is privacy-relevant: a take the user chose
    /// not to submit must not leave audio on disk.
    #[test]
    fn the_diagnostics_classifier_keeps_what_is_worth_debugging() {
        assert_eq!(
            diagnostic_outcome(&Err(Error::Recognition { message: "x".into() })),
            Some(("recognition_error", String::new())),
            "a recognition failure is exactly a take worth opening files over"
        );
        assert_eq!(
            diagnostic_outcome(&Err(Error::Cancelled)),
            None,
            "an abandoned take must persist nothing: no audio, no directory"
        );
        // The silence gate resolves before the worker ever sees a job,
        // so this arm is defensive; the assert documents the intent so
        // a future edit cannot quietly start storing silence.
        assert_eq!(
            diagnostic_outcome(&Ok(Outcome::NoAudio {
                peak: f32::NEG_INFINITY,
                audio: Duration::ZERO
            })),
            None,
            "silence has no transcript stage to examine and no words to lose"
        );
        let empty = Outcome::Transcript(Transcript {
            text: String::new(),
            asr: String::new(),
            stages: Stages::default(),
            truncated: false,
        });
        assert_eq!(
            diagnostic_outcome(&Ok(empty)),
            Some(("empty", String::new())),
            "a take that normalized to nothing still keeps its audio"
        );
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

    #[test]
    fn the_take_aggregate_joins_raw_texts_in_submission_order() {
        let parts = vec![
            Part::Transcript { raw: "first".into(), stages: Stages::default(), truncated: false },
            Part::NoAudio { peak: -40.0, audio: Duration::from_millis(500) },
            Part::Transcript { raw: "second".into(), stages: Stages::default(), truncated: true },
        ];
        match aggregate_parts(parts) {
            Aggregate::Join { raws, failed } => {
                assert!(failed.is_none(), "no part failed");
                assert_eq!(
                    raws,
                    ["first", "second"],
                    "silence contributes nothing to the join; order is the take's order"
                );
                assert_eq!(
                    join_window_texts(&raws),
                    "first second",
                    "the join, not the pieces, is what the normalizer sees"
                );
            }
            other => panic!("a take with recognized parts must join, got {other:?}"),
        }
    }

    #[test]
    fn a_take_with_nothing_above_the_floor_reads_as_silence_over_the_whole_span() {
        let parts = vec![
            Part::NoAudio { peak: -40.0, audio: Duration::from_millis(500) },
            Part::NoAudio { peak: -30.0, audio: Duration::from_millis(1500) },
        ];
        match aggregate_parts(parts) {
            Aggregate::NoAudio { peak, audio } => {
                // Exact, not approximated: the aggregate copies the
                // input peak, it computes nothing.
                #[allow(clippy::float_cmp)]
                {
                    assert_eq!(peak, -30.0, "the take's peak is the loudest segment's");
                }
                assert_eq!(audio, Duration::from_millis(2000), "the span is the whole take");
            }
            other => panic!("a take with no recognized part must read as silence, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_after_a_segment_with_words_keeps_the_words() {
        let parts = vec![
            Part::Transcript { raw: "early".into(), stages: Stages::default(), truncated: false },
            Part::Failed(Error::Recognition { message: "one".into() }),
            Part::Failed(Error::Recognition { message: "two".into() }),
        ];
        match aggregate_parts(parts) {
            Aggregate::Join { raws, failed: Some(Error::Recognition { message }) } => {
                assert_eq!(raws, ["early"], "the words before the failure are kept");
                assert_eq!(
                    message, "one",
                    "the first failure in submission order is the one carried"
                );
            }
            other => {
                panic!("a failure after words must keep them and carry the error, got {other:?}")
            }
        }
    }

    #[test]
    fn a_failure_before_any_words_fails_the_take() {
        let parts = vec![
            Part::NoAudio { peak: -40.0, audio: Duration::from_millis(500) },
            Part::Failed(Error::Recognition { message: "one".into() }),
            Part::Failed(Error::Recognition { message: "two".into() }),
        ];
        match aggregate_parts(parts) {
            Aggregate::Failed(Error::Recognition { message }) => assert_eq!(
                message, "one",
                "the first failure in submission order wins when there is nothing to keep"
            ),
            other => panic!("a failure before any words must fail the take, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_after_only_whitespace_still_fails_the_take() {
        let parts = vec![
            Part::Transcript { raw: " ".into(), stages: Stages::default(), truncated: false },
            Part::Failed(Error::Recognition { message: "one".into() }),
        ];
        match aggregate_parts(parts) {
            Aggregate::Failed(Error::Recognition { message }) => assert_eq!(message, "one"),
            other => panic!("whitespace is not words, got {other:?}"),
        }
    }

    #[test]
    fn a_cancel_after_another_failure_still_discards() {
        let parts = vec![
            Part::Transcript { raw: "early".into(), stages: Stages::default(), truncated: false },
            Part::Failed(Error::Recognition { message: "one".into() }),
            Part::Failed(Error::Cancelled),
        ];
        match aggregate_parts(parts) {
            Aggregate::Failed(Error::Cancelled) => {}
            other => panic!("a cancel anywhere must discard the take, got {other:?}"),
        }
    }

    /// The fold's contract for a stopped engine: the words already
    /// transcribed are kept and the stop is carried as the error. Teardown
    /// is a different path and differs on both sides: with no words it
    /// answers `Err(EngineStopped)` and `finish` skips the write, while with
    /// words the fold keeps them - so what is pinned here is the fold's
    /// input handling, not a teardown guarantee.
    #[test]
    fn the_fold_keeps_prior_words_when_the_engine_stops() {
        let parts = vec![
            Part::Transcript { raw: "early".into(), stages: Stages::default(), truncated: false },
            Part::Failed(Error::EngineStopped),
        ];
        match aggregate_parts(parts) {
            Aggregate::Join { raws, failed: Some(Error::EngineStopped) } => {
                assert_eq!(
                    raws,
                    ["early"],
                    "the fold keeps the words it has and carries the stopped engine"
                );
            }
            other => panic!("the fold must keep words and carry the stopped engine, got {other:?}"),
        }
    }

    #[test]
    fn words_after_a_failed_segment_are_kept() {
        let parts = vec![
            Part::Failed(Error::Recognition { message: "one".into() }),
            Part::Transcript { raw: "later".into(), stages: Stages::default(), truncated: false },
        ];
        match aggregate_parts(parts) {
            Aggregate::Join { raws, failed: Some(Error::Recognition { .. }) } => {
                assert_eq!(raws, ["later"], "later segments' words are kept too");
            }
            other => panic!("words after a failed segment must be kept, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_segment_makes_the_take_truncated() {
        assert!(take_is_truncated(false, false, true), "a failed segment leaves a gap");
        assert!(take_is_truncated(true, false, false), "a segment's own truncation carries");
        assert!(take_is_truncated(false, true, false), "the capture cap carries");
        assert!(!take_is_truncated(false, false, false));
    }

    /// Whether the store holds no take. An absent store and a bare root
    /// both hold none: a take directory is what persistence means, and
    /// the retention scan only counts names that start with `take-`.
    fn no_take_was_stored(store: &Path) -> bool {
        match std::fs::read_dir(store) {
            Ok(entries) => entries
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().starts_with("take-")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => panic!("the store could not be read: {error}"),
        }
    }

    /// A job shaped like the worker's prejoined finalize: no audio, the
    /// take's join as its text, and the token the test hands it.
    fn prejoined_job(cancel: CancelToken) -> Job {
        let (reply, _answer) = channel();
        Job {
            pcm: Vec::new(),
            resample: Duration::ZERO,
            audio: Duration::ZERO,
            truncated: false,
            normalize: Some(NormalizeOptions::default()),
            prejoined: Some("early".to_owned()),
            diagnose: false,
            cancel,
            reply,
            queued_at: Instant::now(),
        }
    }

    /// Drive one live take's finish over a fake worker that runs the same
    /// per-job seam the real worker does: one prejoined job, finished by
    /// [`finish_job`] with no normalizer, so the answer is the join it was
    /// handed. `cancel_on_job` fires the take's token as the worker takes
    /// the join, standing in for a cancel that lands after the last window
    /// has settled. It gives up when the take's segmenter drops its sender.
    fn finish_live_take(
        parts: Vec<Part>,
        cuts: Vec<(usize, usize)>,
        cancel: CancelToken,
        cancel_on_job: bool,
        finish: FinishTake,
        diagnostics_dir: Option<PathBuf>,
    ) -> Receiver<Result<Outcome, Error>> {
        let (jobs, answers) = channel::<Job>();
        let worker_cancel = cancel.clone();
        std::thread::spawn(move || {
            while let Ok(job) = answers.recv() {
                if cancel_on_job {
                    worker_cancel.cancel();
                }
                // The prejoined text is the take's join, so the seam builds
                // the answer from it exactly as the worker would. Echoing
                // rather than inventing the text is what makes a landed
                // transcript assert the join.
                let parts: Vec<String> = job.prejoined.clone().into_iter().collect();
                let out = finish_job(&job, &parts, None, false, Stages::default(), None);
                let _ = job.reply.send(out.answer);
            }
        });
        let (answer_tx, answer_rx) = channel();
        let (progress_tx, _progress_rx) = channel();
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigBuilder::new().models_dir(dir.path()).normalizer(None).build();
        // The parts ARE the settled segments, so the recording holds exactly
        // their audio and the cut cursor sits at its end: finish sees no
        // tail, and a record built here names the spans it was handed rather
        // than ones production never produces.
        let recorded = cuts.last().map_or(0, |&(_, end)| end);
        let recording = crate::capture::Recording::new(recorded.max(1));
        recording.push(&vec![0.5; recorded], recorded.max(1));
        let settled = parts.len();
        let failed = parts.iter().enumerate().find_map(|(index, part)| match part {
            Part::Failed(error) => Some((index, error.to_string())),
            Part::Transcript { .. } | Part::NoAudio { .. } => None,
        });
        let segmenter = TakeSegmenter {
            recording: Arc::new(recording),
            jobs,
            silence_floor: -50.0,
            diagnostics_dir,
            models: Arc::new(diagnostics::TakeModels::from_config(&cfg)),
            take: TakeShared { cancel, answer: answer_tx, progress: progress_tx },
            last_cut: recorded,
            cuts,
            parts,
            pending: Vec::new(),
            settled,
            first_queued_at: None,
            first_settled_at: None,
            failed,
        };
        segmenter.finish(finish);
        answer_rx
    }

    /// The live path itself must mark the take truncated when a segment
    /// failed after words landed: the client's "part of the take is
    /// missing" copy depends on it, and dropping the flag would render a
    /// mid-take gap as complete.
    ///
    /// Two windows of words, so the answer has to carry the join: a worker
    /// that ignores the joined text passes on a single window alone.
    #[test]
    fn a_failed_segment_marks_the_live_transcript_truncated() {
        let answer = finish_live_take(
            vec![
                Part::Transcript {
                    raw: "early".into(),
                    stages: Stages::default(),
                    truncated: false,
                },
                Part::Transcript {
                    raw: "later".into(),
                    stages: Stages::default(),
                    truncated: false,
                },
                Part::Failed(Error::Recognition { message: "one".into() }),
            ],
            vec![(0, 80), (80, 160), (160, 180)],
            CancelToken::new(),
            false,
            FinishTake { options: NormalizeOptions::default(), truncated: false },
            None,
        );
        match answer.recv().expect("the take must answer") {
            Ok(Outcome::Transcript(transcript)) => {
                assert!(transcript.truncated, "a failed segment leaves a gap");
                assert_eq!(transcript.text, "early later", "the take lands the join it built");
            }
            other => panic!("a take with words and a failed segment must finalize, got {other:?}"),
        }
    }

    /// The diagnostics record's own truncated flag on a failed segment:
    /// `failed_before_join` is one of that field's three inputs, and no
    /// other test asserts the stored value.
    #[test]
    fn a_failed_segment_records_a_truncated_take() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let answer = finish_live_take(
            vec![
                Part::Transcript {
                    raw: "early".into(),
                    stages: Stages::default(),
                    truncated: false,
                },
                Part::Failed(Error::Recognition { message: "one".into() }),
            ],
            vec![(0, 80), (80, 160)],
            CancelToken::new(),
            false,
            FinishTake { options: NormalizeOptions::default(), truncated: false },
            Some(store.clone()),
        );
        assert!(
            matches!(answer.recv().expect("the take must answer"), Ok(Outcome::Transcript(_))),
            "a take with words and a failed segment must finalize"
        );
        let take = std::fs::read_dir(&store)
            .expect("the store")
            .flatten()
            .next()
            .expect("the failed take still wrote its record")
            .path();
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(take.join("meta.json")).unwrap())
                .expect("meta.json parses");
        assert_eq!(meta["truncated"], true, "a failed segment leaves a gap the record has to name");
    }

    /// A segment that stopped short (the decode budget, a repetition loop)
    /// reaches the live answer as a truncated take, not only as a per-window
    /// flag: the client's "part of the take is missing" copy rides this
    /// input to `take_is_truncated`.
    #[test]
    fn a_truncated_segment_marks_the_live_transcript_truncated() {
        let answer = finish_live_take(
            vec![Part::Transcript {
                raw: "early".into(),
                stages: Stages::default(),
                truncated: true,
            }],
            vec![(0, 80)],
            CancelToken::new(),
            false,
            FinishTake { options: NormalizeOptions::default(), truncated: false },
            None,
        );
        match answer.recv().expect("the take must answer") {
            Ok(Outcome::Transcript(transcript)) => assert!(
                transcript.truncated,
                "a segment that stopped short leaves a gap in the take"
            ),
            other => panic!("a take with words must finalize, got {other:?}"),
        }
    }

    /// The capture cap is the other input to the same flag: a take cut by
    /// `max_capture` lands truncated even when every segment it did run
    /// came back whole.
    #[test]
    fn a_capped_take_marks_the_live_transcript_truncated() {
        let answer = finish_live_take(
            vec![Part::Transcript {
                raw: "early".into(),
                stages: Stages::default(),
                truncated: false,
            }],
            vec![(0, 80)],
            CancelToken::new(),
            false,
            FinishTake { options: NormalizeOptions::default(), truncated: true },
            None,
        );
        match answer.recv().expect("the take must answer") {
            Ok(Outcome::Transcript(transcript)) => assert!(
                transcript.truncated,
                "the capture cap cuts the take short even when no segment stopped short"
            ),
            other => panic!("a take with words must finalize, got {other:?}"),
        }
    }

    /// The flag's false branch, on a clean landed take: every other live
    /// take test expects `true`, so a predicate that always reported a gap
    /// would pass them all.
    #[test]
    fn a_clean_take_lands_untruncated() {
        let answer = finish_live_take(
            vec![Part::Transcript {
                raw: "early".into(),
                stages: Stages::default(),
                truncated: false,
            }],
            vec![(0, 80)],
            CancelToken::new(),
            false,
            FinishTake { options: NormalizeOptions::default(), truncated: false },
            None,
        );
        match answer.recv().expect("the take must answer") {
            Ok(Outcome::Transcript(transcript)) => {
                assert!(!transcript.truncated, "a take that never stopped short lands whole");
            }
            other => panic!("a take with words must finalize, got {other:?}"),
        }
    }

    /// A cancel can land after the take's last window has settled, while the
    /// take is still transcribing. The only work left is the join, and a
    /// prejoined job never calls `session.run`, the one place the token is
    /// observed: without a check at the boundary the abandoned take answers
    /// with its words and persists them.
    #[test]
    fn a_cancel_after_the_last_window_persists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let cancel = CancelToken::new();
        cancel.cancel();
        let answer = finish_live_take(
            vec![Part::Transcript {
                raw: "early".into(),
                stages: Stages::default(),
                truncated: false,
            }],
            vec![(0, 80)],
            cancel,
            false,
            FinishTake { options: NormalizeOptions::default(), truncated: false },
            Some(store.clone()),
        );
        match answer.recv().expect("the take must answer") {
            Err(Error::Cancelled) => {}
            other => panic!("an abandoned take must answer cancelled, got {other:?}"),
        }
        assert!(no_take_was_stored(&store), "an abandoned take persists nothing");
    }

    /// A cancel that lands after the segmenter's own check, while the join
    /// is already queued: the seam reads the token again and answers
    /// cancelled rather than the words it was handed, and the write keeps
    /// nothing.
    #[test]
    fn a_cancel_during_the_join_persists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let answer = finish_live_take(
            vec![Part::Transcript {
                raw: "early".into(),
                stages: Stages::default(),
                truncated: false,
            }],
            vec![(0, 80)],
            CancelToken::new(),
            true,
            FinishTake { options: NormalizeOptions::default(), truncated: false },
            Some(store.clone()),
        );
        match answer.recv().expect("the take must answer") {
            Err(Error::Cancelled) => {}
            other => panic!("a cancelled join must answer cancelled, got {other:?}"),
        }
        assert!(no_take_was_stored(&store), "an abandoned take persists nothing");
    }

    /// The read after the join's normalize pass: a cancel that lands inside
    /// it is invisible to the boundary read, and this phase is entered with
    /// the token already set. It must answer cancelled, not the words the
    /// pass rewrote.
    #[test]
    fn a_cancel_inside_the_normalize_pass_answers_cancelled() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let job = prejoined_job(cancel);
        let out = finish_after_join(
            &job,
            "early".to_owned(),
            "early".to_owned(),
            Stages::default(),
            false,
        );
        assert!(
            matches!(out.answer, Err(Error::Cancelled)),
            "a cancel inside the normalize pass must abort the answer, got {:?}",
            out.answer
        );
        assert_eq!(out.diag, None, "an abandoned take keeps no record");
    }

    /// The segmenter's early check on the arm only it guards: a take whose
    /// every segment failed has no words, so without the token read the
    /// answer is the recognition error and `diagnostic_outcome` keeps a
    /// record. An abandoned take must answer cancelled and write nothing.
    #[test]
    fn a_cancelled_take_with_no_words_persists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let cancel = CancelToken::new();
        cancel.cancel();
        let answer = finish_live_take(
            vec![Part::Failed(Error::Recognition { message: "one".into() })],
            vec![(0, 80)],
            cancel,
            false,
            FinishTake { options: NormalizeOptions::default(), truncated: false },
            Some(store.clone()),
        );
        match answer.recv().expect("the take must answer") {
            Err(Error::Cancelled) => {}
            other => panic!("an abandoned take must answer cancelled, got {other:?}"),
        }
        assert!(no_take_was_stored(&store), "an abandoned take persists nothing");
    }

    /// The write's own read, for a worker job whose answer was already
    /// built when the token was cancelled: the classifier alone would keep
    /// a record, so both write sites go through `kept_diagnostics`.
    #[test]
    fn a_cancelled_take_keeps_no_record_whatever_its_answer_says() {
        let answer = Ok(Outcome::Transcript(Transcript {
            text: "early".to_owned(),
            asr: "early".to_owned(),
            stages: Stages::default(),
            truncated: false,
        }));
        let cancelled = CancelToken::new();
        cancelled.cancel();
        assert_eq!(
            kept_diagnostics(&cancelled, &answer),
            None,
            "an abandoned take's words are never kept, even after the answer is built"
        );
        let live = CancelToken::new();
        assert_eq!(
            kept_diagnostics(&live, &answer).map(|(outcome, _)| outcome),
            Some("transcript"),
            "a live take keeps its record"
        );
    }

    /// The worker's own half of the boundary, driven over a prejoined job
    /// with no model: a take cancelled after its last window has no
    /// `session.run` left to abort, so `finish_job` must read the token and
    /// answer cancelled rather than build the words. It takes the job
    /// itself, so the token it reads is the job's own by construction and
    /// the worker cannot wire another.
    #[test]
    fn a_cancelled_prejoined_job_answers_cancelled() {
        let cancelled = CancelToken::new();
        cancelled.cancel();
        let job = prejoined_job(cancelled);
        let stages = Stages { audio: Duration::from_millis(1500), ..Stages::default() };
        let out = finish_job(&job, &["early".to_owned()], None, false, stages.clone(), None);
        assert!(
            matches!(out.answer, Err(Error::Cancelled)),
            "a cancelled take with a clean fold must not answer with its words, got {:?}",
            out.answer
        );
        assert_eq!(out.stages, stages, "a cancelled job carries the stages it was given");
        assert_eq!(out.diag, None, "an abandoned take keeps no record");

        // A cancel outranks a failure the fold already carries, the same
        // rule the take fold applies.
        let cancelled = CancelToken::new();
        cancelled.cancel();
        let job = prejoined_job(cancelled);
        let out = finish_job(
            &job,
            &[],
            Some(Error::Recognition { message: "one".into() }),
            false,
            Stages::default(),
            None,
        );
        assert!(
            matches!(out.answer, Err(Error::Cancelled)),
            "a cancel outranks the fold's own failure, got {:?}",
            out.answer
        );

        // A live token builds the answer from the prejoined text, which is
        // what the worker does with no normalizer configured.
        let job = prejoined_job(CancelToken::new());
        let out = finish_job(&job, &["early".to_owned()], None, false, Stages::default(), None);
        match out.answer {
            Ok(Outcome::Transcript(transcript)) => {
                assert_eq!(transcript.text, "early", "the prejoined text is the answer");
                assert_eq!(transcript.asr, "early", "and its raw recognition");
            }
            other => panic!("a live token must build the transcript, got {other:?}"),
        }
        assert!(out.diag.is_some(), "a landed take keeps its record");
    }

    #[test]
    fn a_take_without_cuts_still_resolves_through_the_pipeline() {
        let (_dir, engine) = engine_without_weights();
        let stream = engine.capture_stream().expect("an idle engine must take a stream");
        for chunk in loud(5).chunks(SAMPLE_RATE as usize) {
            assert!(stream.sink.push_mono(chunk), "a push below the cap must be kept");
        }
        let ticket = stream.capture.finish().expect("the take must finish");
        assert!(matches!(ticket.recv(), Err(Error::ModelLoad { .. })));
    }

    #[test]
    fn abandoning_a_stream_take_mid_recording_does_not_hang() {
        let (_dir, engine) = engine_without_weights();
        let mut stream = engine.capture_stream().expect("an idle engine must take a stream");
        let mut progress =
            stream.capture.take_progress().expect("the capture carries a progress stream");
        for chunk in loud(70).chunks(SAMPLE_RATE as usize) {
            assert!(stream.sink.push_mono(chunk), "a push below the cap must be kept");
        }
        wait_for_a_settled_segment(&mut progress);
        drop(stream);
        engine.capture_stream().expect("the abandon must leave the engine able to take another");
    }

    #[test]
    fn dropping_the_ticket_mid_pipeline_cancels_without_hanging() {
        let (_dir, engine) = engine_without_weights();
        let mut stream = engine.capture_stream().expect("an idle engine must take a stream");
        let mut progress =
            stream.capture.take_progress().expect("the capture carries a progress stream");
        for chunk in loud(70).chunks(SAMPLE_RATE as usize) {
            assert!(stream.sink.push_mono(chunk), "a push below the cap must be kept");
        }
        wait_for_a_settled_segment(&mut progress);
        let ticket = stream.capture.finish().expect("the take must finish");
        drop(ticket);
        engine
            .capture_stream()
            .expect("the cancelled take must leave the engine able to take another");
    }

    /// The stream-fed take, weightless: pushed samples cross the window
    /// ceiling, the segmenter cuts and queues while the take is still
    /// open, and the take answers through the pipeline.
    #[test]
    fn a_segment_settles_while_the_stream_is_still_open() {
        let (_dir, engine) = engine_without_weights();
        let mut stream = engine.capture_stream().expect("an idle engine must take a stream");
        let mut progress =
            stream.capture.take_progress().expect("the capture carries a progress stream");
        for chunk in loud(70).chunks(SAMPLE_RATE as usize) {
            assert!(stream.sink.push_mono(chunk), "a push below the cap must be kept");
        }
        let seen = wait_for_a_settled_segment(&mut progress);
        assert!(
            seen.iter().all(|step| step.total.is_none()),
            "a live take cannot know its total, got {seen:?}"
        );
        let ticket = stream.capture.finish().expect("the take must finish");
        assert!(
            matches!(ticket.recv(), Err(Error::ModelLoad { .. })),
            "the pushed audio reached the worker, so its failure is the take's answer"
        );
    }

    /// A stream-fed take meters and caps: the peak read is the pushed
    /// samples' own, and the cap stops the take rather than letting the
    /// buffer grow without bound.
    #[test]
    fn a_stream_take_meters_and_caps_what_it_is_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .normalizer(None)
            .max_capture(Duration::from_secs(1))
            .build();
        let engine = Engine::new(cfg).expect("engine must start");
        let stream = engine.capture_stream().expect("an idle engine must take a stream");

        assert!(stream.sink.push_mono(&[0.5; 160]), "a push below the cap must be kept");
        let peak = stream.capture.meter().level();
        assert!((peak + 6.02).abs() < 0.1, "the meter must read the pushed samples, got {peak}");
        assert!(!stream.capture.was_truncated(), "a take inside its cap is not truncated");

        for _ in 0..3 {
            stream.sink.push_mono(&vec![0.5; SAMPLE_RATE as usize]);
        }
        assert!(
            stream.capture.was_truncated(),
            "reaching the cap must flag the take, or a short transcript reads as a short utterance"
        );
        assert!(!stream.sink.push_mono(&[0.5; 16]), "a stopped take keeps nothing further");
        let ticket = stream.capture.finish().expect("a capped take still finishes");
        assert!(matches!(ticket.recv(), Err(Error::ModelLoad { .. })));
    }

    /// The only refusal a stream take can meet is an engine that is
    /// stopping, because there is no device to open and no holder to
    /// contend with. A take accepted during teardown would be a take
    /// whose every job the drain discards.
    #[test]
    fn a_stream_take_on_a_stopped_engine_is_refused() {
        let (_dir, engine) = engine_without_weights();
        let stream = engine.capture_stream().expect("an idle engine must take a stream");
        engine.stopping.store(true, Ordering::Relaxed);
        assert!(
            matches!(engine.capture_stream(), Err(Error::EngineStopped)),
            "a stopping engine must refuse a new take rather than queue work it will discard"
        );
        drop(stream);
    }

    /// The wiring leg for a capture's provenance: its record names the
    /// models the engine was BUILT with, not a default or a stub.
    ///
    /// Weightless: the failed load answers the take's segments, and a
    /// take whose segments failed still writes its record - which is
    /// the whole path a live capture's provenance rides.
    #[test]
    fn a_live_take_records_the_models_the_engine_was_built_with() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(ModelSpec {
                file: "asr-stub.gguf".into(),
                url: String::new(),
                size: 4242,
                sha256: Some("a1b2c3".into()),
                facts: ModelFacts::default(),
            })
            .normalizer(None)
            .language("en")
            .diagnostics_dir(&store)
            .build();
        let engine = Engine::new(cfg).expect("engine must start");
        let stream = engine.capture_stream().expect("an idle engine must take a stream");
        for chunk in loud(5).chunks(SAMPLE_RATE as usize) {
            assert!(stream.sink.push_mono(chunk), "a push below the cap must be kept");
        }
        let ticket = stream.capture.finish().expect("the take must finish");
        // `recv` joins the segmenter, which writes the record before it
        // returns, so the store is complete once this lands.
        assert!(
            matches!(ticket.recv(), Err(Error::ModelLoad { .. })),
            "the missing weights' failure is the take's answer"
        );

        let take = std::fs::read_dir(&store)
            .expect("the store directory")
            .flatten()
            .next()
            .expect("a failed take still writes its record")
            .path();
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(take.join("meta.json")).unwrap())
                .expect("meta.json parses");
        assert_eq!(meta["outcome"], "recognition_error", "the take's own outcome");
        assert_eq!(
            meta["asr"]["file"], "asr-stub.gguf",
            "the record names the model this engine was built with"
        );
        assert_eq!(meta["asr"]["sha256"], "a1b2c3", "its declared digest");
        assert_eq!(meta["asr"]["size"], 4242, "its declared size");
        assert!(meta["normalizer"].is_null(), "no configured cleanup names no model");
        assert_eq!(meta["normalizer_ran"], false, "nothing normalized this take");
        assert_eq!(meta["language"], "en", "the language the engine was built with");
        assert_eq!(
            meta["forge_version"],
            env!("CARGO_PKG_VERSION"),
            "the release the take was made under"
        );
    }

    /// Poll a capture's progress stream until one segment has settled,
    /// while the recording is still open.
    fn wait_for_a_settled_segment(progress: &mut Receiver<WindowProgress>) -> Vec<WindowProgress> {
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            while let Ok(step) = progress.try_recv() {
                seen.push(step);
            }
            if seen.iter().any(|step| step.done >= 1) {
                return seen;
            }
            assert!(
                Instant::now() < deadline,
                "no segment settled while the recording was open: {seen:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Well above any sane silence floor, so fed audio becomes jobs
    /// rather than silence-gated skips.
    fn loud(seconds_long: usize) -> Vec<f32> {
        vec![0.5; seconds_long * SAMPLE_RATE as usize]
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

    /// The live cut rule and whole-buffer windowing are one function,
    /// [`next_cut`]; this pins the consequence, that deciding each cut
    /// as the buffer grows lands exactly where deciding after the take
    /// would. Deterministic dips - one quiet frame per second - so the
    /// cut is the latest quiet frame the search window sees.
    #[test]
    fn live_cuts_and_whole_buffer_cuts_are_the_same_function() {
        let mut pcm = Vec::new();
        for _ in 0..360 {
            pcm.extend(std::iter::repeat_n(0.4, ENERGY_FRAME * 49));
            pcm.extend(std::iter::repeat_n(0.0, ENERGY_FRAME));
        }
        let offline = window_bounds(&pcm);
        assert!(
            offline.len() > 2,
            "a six-minute take must span several windows, got {}",
            offline.len()
        );

        let mut live: Vec<(usize, usize)> = Vec::new();
        let mut last_cut = 0;
        for end in (0..=pcm.len()).step_by(SAMPLE_RATE as usize) {
            while let Some(cut) = next_cut(&pcm[..end], last_cut) {
                live.push((last_cut, cut));
                last_cut = cut;
            }
        }
        // The tail is the finish-time submission.
        live.push((last_cut, pcm.len()));
        assert_eq!(
            live, offline,
            "deciding cuts as the buffer grows must land exactly where whole-buffer windowing does"
        );
    }
}

#[cfg(test)]
mod tests_window_outcomes {
    use super::*;

    fn words(text: &str) -> transcribe_cpp::Transcript {
        transcribe_cpp::Transcript { text: text.to_owned(), ..Default::default() }
    }

    fn repetition(text: &str) -> transcribe_cpp::Error {
        transcribe_cpp::Error::OutputRepetition {
            message: "output began repeating".to_owned(),
            partial: Some(Box::new(words(text))),
        }
    }

    fn failure() -> transcribe_cpp::Error {
        transcribe_cpp::Error::Other("decode failed".to_owned())
    }

    /// A repetition stop still carries the words the recognizer kept. Dropping
    /// them into the generic error arm fails the whole take and loses every
    /// window already transcribed.
    #[test]
    fn an_output_repetition_with_a_partial_keeps_the_text() {
        assert_eq!(
            window_action(Err(repetition("the words so far")), false),
            WindowAction::Truncated("the words so far".to_owned()),
            "repetition with a partial must keep the text and mark the take truncated, not fail it"
        );
    }

    /// A window that fails after earlier windows produced words must not
    /// discard them: the take finalizes what it has and carries the error.
    #[test]
    fn a_generic_error_after_good_windows_keeps_the_prior_text() {
        assert_eq!(
            window_action(Err(failure()), true),
            WindowAction::FinalizeWithError { message: "decode failed".to_owned() },
            "a window failure after good windows must finalize what was transcribed, not discard it"
        );
    }

    /// With nothing to finalize, the take fails - and a failure is what the
    /// client draws on the status row.
    #[test]
    fn a_first_window_error_fails_visibly() {
        assert_eq!(
            window_action(Err(failure()), false),
            WindowAction::Failed { message: "decode failed".to_owned() },
            "with no text to keep, the take must fail so the client draws the failure notice"
        );
    }

    /// The loop's side effects, not just the classification: a
    /// finalize-with-error must keep the take alive and mark it truncated.
    /// Merging its arm with `Failed` keeps the classification tests green
    /// while re-losing the words, so this pins the transition itself.
    #[test]
    fn a_finalize_with_error_does_not_fail_the_take() {
        let mut state = TakeState::new(false);
        let keep_going = state.apply(
            WindowAction::FinalizeWithError { message: "decode failed".to_owned() },
            1,
            0,
            1000,
        );
        assert!(!keep_going, "the loop stops after a finalize");
        assert!(state.failure.is_none(), "a finalize must not fail the take");
        assert!(state.truncated, "a finalize must mark the take truncated");
        assert_eq!(state.recognition_error, Some((1, "decode failed".to_owned())));
    }

    #[test]
    fn a_failed_action_fails_the_take() {
        let mut state = TakeState::new(false);
        let keep_going =
            state.apply(WindowAction::Failed { message: "decode failed".to_owned() }, 0, 0, 1000);
        assert!(!keep_going);
        assert!(
            matches!(
                state.failure,
                Some(Error::Recognition { ref message }) if message == "decode failed"
            ),
            "a failed window must fail the take with its own error"
        );
        assert_eq!(
            state.recognition_error,
            Some((0, "decode failed".to_owned())),
            "the failure carries its window and message to diagnostics"
        );
        assert!(!state.truncated, "a failed first window is not truncated");
    }

    #[test]
    fn a_truncated_window_keeps_its_partial_and_keeps_going() {
        let mut state = TakeState::new(false);
        assert!(state.apply(WindowAction::Continue("early".to_owned()), 0, 0, 1000));
        assert!(state.apply(WindowAction::Truncated("so far".to_owned()), 1, 1000, 2000));
        assert_eq!(state.asr_parts, ["early", "so far"]);
        assert!(state.truncated, "a truncated window marks the take truncated");
        assert!(state.failure.is_none(), "a truncated window does not fail the take");
        assert_eq!(state.window_records.len(), 2);
        assert_eq!(state.window_records[0].raw, "early", "each record keeps its own raw text");
        assert_eq!(
            state.window_records[1].raw, "so far",
            "a truncated window's partial text is the record's raw"
        );
        assert_eq!(
            (state.window_records[1].start_ms, state.window_records[1].end_ms),
            (1000, 2000),
            "the record carries the window's own span"
        );
    }

    #[test]
    fn whitespace_is_not_prior_text() {
        let mut state = TakeState::new(false);
        state.asr_parts.push(" ".to_owned());
        assert!(!state.has_prior_text(), "whitespace is not words");
        state.asr_parts.push("word".to_owned());
        assert!(state.has_prior_text());
    }

    #[test]
    fn a_cancelled_action_stops_without_words() {
        let mut state = TakeState::new(false);
        assert!(!state.apply(WindowAction::Cancelled, 0, 0, 1000));
        assert!(matches!(state.failure, Some(Error::Cancelled)));
        assert!(state.asr_parts.is_empty(), "a cancel adds no words");
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
    use crate::{ConfigBuilder, ModelSpec, Samples};

    fn read_wav(path: &std::path::Path) -> (Vec<f32>, u32) {
        let bytes = std::fs::read(path).expect("fixture must be readable");
        // Canonical 16-bit PCM: the fixtures are written that way and the
        // manifest's integrity gate keeps them that way.
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let data = bytes[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| f32::from(i16::from_le_bytes(*p)) / 32768.0)
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
        // The store entry is written after the reply, by design, so
        // dropping the engine (which joins the worker) is the reliable
        // way to observe the write. Its own stopping flag is set first,
        // but the write check already ran on this job's answer, so the
        // join only ever waits out a write that started.
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

        let windows = meta["windows"].as_array().expect("windows array");
        assert_eq!(windows.len(), 1, "a sub-window take records its one window");
        let raw = std::fs::read_to_string(take.join("raw/0.txt")).expect("window 0's transcript");
        assert!(!raw.is_empty(), "a spoken window produces a non-empty raw transcript");

        let mut reader = hound::WavReader::open(take.join("output.wav")).unwrap();
        assert_eq!(
            reader.spec().sample_rate,
            SAMPLE_RATE,
            "the capture is stored at the model rate"
        );
        assert_eq!(reader.samples::<i16>().count(), pcm.len(), "the whole capture is stored");
    }

    /// The direct path's wiring leg and its ordering pin: the record
    /// names the models the engine was BUILT with, and is built AFTER
    /// the cleanup pass - `Stages::normalize` is set by the pass itself,
    /// so a snapshot taken before it reports `normalizer_ran` false on a
    /// take that was cleaned.
    ///
    /// Weights-only: the failed-load path answers jobs without writing a
    /// record, so no weightless take reaches the worker's record site.
    #[test]
    #[ignore = "needs both models; run with --run-ignored all after `--example fetch`"]
    fn a_direct_take_records_the_models_and_the_cleanup_pass_it_ran() {
        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/08_009s.wav");
        let (pcm, rate) = read_wav(&clip);
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigBuilder::new().language("en").diagnostics_dir(dir.path()).build();
        let engine = Engine::new(cfg).expect("engine must start");
        let outcome = engine
            .transcribe(Samples::new(pcm, rate, 1))
            .expect("queued")
            .recv()
            .expect("recognition must succeed");
        let Outcome::Transcript(transcript) = outcome else {
            panic!("a spoken take must not read as silence: {outcome:?}");
        };
        assert!(
            transcript.stages.normalize.is_some(),
            "the default config runs cleanup, or this test pins nothing"
        );
        // The write lands after the reply, on the worker the engine drop
        // joins.
        drop(engine);

        let take = std::fs::read_dir(dir.path())
            .expect("store dir")
            .flatten()
            .next()
            .expect("the take wrote its record")
            .path();
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(take.join("meta.json")).unwrap())
                .expect("meta.json parses");
        assert_eq!(
            meta["normalizer_ran"], true,
            "the record carries the pass that ran, not a pre-pass snapshot"
        );
        let asr = ModelSpec::cohere_transcribe_q4_k_m();
        let s1 = ModelSpec::s1_mini_f16();
        assert_eq!(meta["asr"]["file"], asr.file, "the transcribing model the engine ran");
        assert_eq!(meta["asr"]["sha256"], asr.sha256.clone().unwrap(), "its declared digest");
        assert_eq!(meta["asr"]["size"], asr.size, "its declared size");
        assert_eq!(meta["normalizer"]["file"], s1.file, "the cleanup model the engine ran");
        assert_eq!(meta["normalizer"]["sha256"], s1.sha256.clone().unwrap(), "its declared digest");
        assert_eq!(meta["normalizer"]["size"], s1.size, "its declared size");
        assert_eq!(
            meta["language"], "en",
            "the record carries the engine's own config, not a default"
        );
        assert_eq!(
            meta["forge_version"],
            env!("CARGO_PKG_VERSION"),
            "the release the take was made under"
        );
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
