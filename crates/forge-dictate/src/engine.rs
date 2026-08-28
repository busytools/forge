//! The transcription engine and the tickets it hands back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use transcribe_cpp::{CancelToken, Feature, Model, RunOptions};

use crate::audio::{AudioSource, SAMPLE_RATE};
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
    /// Rewriting recognition output into clean text. Zero when no
    /// normalizer is configured.
    pub normalize: Duration,
    /// How much audio this run consumed, so a caller can derive a
    /// realtime factor without knowing where the audio came from.
    pub audio: Duration,
}

/// Clean text, plus what it was before normalization.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    /// The text a caller should use.
    pub text: String,
    /// Recognition output before normalization. Equal to `text` when no
    /// normalizer ran. Deliberately not named `raw`, which the
    /// recognition library already uses for something else.
    pub asr: String,
    /// Where the time went.
    pub stages: Stages,
    /// The recording hit [`Config::max_capture`] and was cut short, so
    /// this is what fitted rather than what was said. Carried here
    /// rather than only logged: a host cannot offer to keep going, or
    /// say "stopped at two minutes", from a log line it may not be
    /// routing.
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
        /// Loudest sample seen, in dBFS. Negative infinity for digital
        /// silence.
        peak: f32,
        /// How much audio was heard, so a caller can say "four seconds of
        /// nothing" rather than just "nothing".
        audio: Duration,
    },
}

/// Samples to wall-clock at the one rate the models accept. Integer
/// maths so a long buffer cannot drift through a float cast.
fn audio_duration(samples: usize) -> Duration {
    Duration::from_micros((samples as u64).saturating_mul(1_000_000) / u64::from(SAMPLE_RATE))
}

/// One queued transcription.
struct Job {
    pcm: Vec<f32>,
    resample: Duration,
    audio: Duration,
    truncated: bool,
    cancel: CancelToken,
    reply: Sender<Result<Outcome, Error>>,
}

/// Loads the models and runs transcriptions, one at a time.
///
/// The recognition session needs `&mut self` to run, so one set of
/// weights means one caller at a time whatever the API looks like.
pub struct Engine {
    max_capture: Duration,
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
        let silence_floor = cfg.silence_floor;
        let stopping = Arc::new(AtomicBool::new(false));
        let in_flight: Arc<Mutex<Option<CancelToken>>> = Arc::new(Mutex::new(None));
        let (jobs, queue) = channel();

        let handle = std::thread::Builder::new()
            .name("forge-dictate".into())
            .spawn({
                let stopping = Arc::clone(&stopping);
                let in_flight = Arc::clone(&in_flight);
                move || worker(&asr_path, &cfg, &queue, &stopping, &in_flight)
            })
            .map_err(|source| Error::WorkerSpawn { message: source.to_string() })?;

        Ok(Arc::new(Engine {
            max_capture,
            silence_floor,
            stopping,
            in_flight,
            jobs: Some(jobs),
            worker: Some(handle),
            holder: Arc::new(Mutex::new(None)),
        }))
    }

    /// Queue `source` for transcription.
    ///
    /// Rejects a source the models cannot read before queueing anything.
    /// A wrong sample rate or channel count is a caller mistake, knowable
    /// the moment the source is handed over, so it is refused here rather
    /// than arriving later mixed in with real runtime failures.
    pub fn transcribe(&self, mut source: impl AudioSource) -> Result<Ticket, Error> {
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

        self.submit(pcm, resample, false)
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
        let recorder = std::thread::Builder::new()
            .name("forge-dictate-mic".into())
            .spawn(move || crate::capture::record(&shared, max_capture, &ready))
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
    fn submit(&self, pcm: Vec<f32>, resample: Duration, truncated: bool) -> Result<Ticket, Error> {
        let (reply, answer) = channel();
        let cancel = CancelToken::new();

        // Silence is a property of the samples, so it is decided here
        // rather than on the worker: a quiet capture needs no weights,
        // should not load any, and should not queue behind a backlog.
        let peak = Self::peak_dbfs(&pcm);
        let audio = audio_duration(pcm.len());
        if peak < self.silence_floor {
            let _ = reply.send(Ok(Outcome::NoAudio { peak, audio }));
            return Ok(Ticket { answer, cancel });
        }

        self.jobs
            .as_ref()
            .ok_or(Error::EngineStopped)?
            .send(Job { pcm, resample, audio, truncated, cancel: cancel.clone(), reply })
            .map_err(|_| Error::EngineStopped)?;
        Ok(Ticket { answer, cancel })
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
    /// Loudest input so far, in dBFS. A lock-free atomic read, so it is
    /// safe to call from a render loop.
    pub fn level(&self) -> f32 {
        self.recording.peak_dbfs()
    }

    /// Stop recording and queue what was captured. Releases the
    /// microphone before the transcription starts, so the next caller
    /// does not wait for inference.
    pub fn finish(mut self) -> Result<Ticket, Error> {
        if let Some(error) = self.failed_to_open.take() {
            return Err(error);
        }
        let pcm = self.stop_recording();
        let truncated = self.recording.was_truncated();
        if truncated {
            tracing::warn!(cap = ?self.max_capture, "capture reached its cap and stopped itself");
        }
        self.engine.submit(pcm, Duration::ZERO, truncated)
    }

    /// Stop recording and throw the audio away.
    pub fn cancel(self) {
        drop(self);
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
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Owns the weights and drains the queue.
fn worker(
    asr_path: &std::path::Path,
    cfg: &Config,
    queue: &Receiver<Job>,
    stopping: &AtomicBool,
    in_flight: &Mutex<Option<CancelToken>>,
) {
    // Routed here rather than in `Engine::new` so the two cannot drift
    // apart: this is the only place a model is loaded, and suppression
    // has to precede that.
    ROUTE_NATIVE_LOGS.call_once(transcribe_cpp::init_logging);

    let started = Instant::now();
    let loaded = Model::load(asr_path).and_then(|model| model.session().map(|s| (model, s)));
    let model_load = started.elapsed();

    let (model, mut session) = match loaded {
        Ok(pair) => pair,
        Err(source) => {
            // Every waiting caller hears the same thing, rather than
            // blocking forever on a worker that never started.
            let message = source.to_string();
            while let Ok(job) = queue.recv() {
                let _ = job.reply.send(Err(Error::ModelLoad {
                    path: asr_path.into(),
                    message: message.clone(),
                }));
            }
            return;
        }
    };

    // Loaded here, on the worker, so a second set of weights never lands
    // on the caller's thread.
    let normalizer = match cfg.normalizer.as_ref() {
        None => None,
        Some(spec) => {
            let path = asr_path.with_file_name(&spec.file);
            match crate::normalize::Normalizer::load(&path) {
                Ok(normalizer) => Some(normalizer),
                Err(source) => {
                    let message = source.to_string();
                    while let Ok(job) = queue.recv() {
                        let _ = job.reply.send(Err(Error::ModelLoad {
                            path: path.clone(),
                            message: message.clone(),
                        }));
                    }
                    return;
                }
            }
        }
    };

    let cancellable = model.supports(Feature::Cancellation);
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
        in_flight.lock().unwrap_or_else(std::sync::PoisonError::into_inner).replace(job.cancel);
        let answer = match session.run(&job.pcm, &options) {
            Ok(out) => {
                stages.mel = Duration::from_secs_f64(f64::from(out.timings.mel_ms) / 1000.0);
                stages.encode = Duration::from_secs_f64(f64::from(out.timings.encode_ms) / 1000.0);
                stages.decode = Duration::from_secs_f64(f64::from(out.timings.decode_ms) / 1000.0);
                let asr = out.text.trim().to_owned();
                // A normalizer that fails mid-session must not cost the
                // speaker their words: fall back to the recognised text
                // and say so, where a load failure above is fatal.
                let text = match normalizer.as_ref() {
                    None => asr.clone(),
                    Some(normalizer) => {
                        let started = Instant::now();
                        match normalizer.normalize(&asr) {
                            Ok(clean) => {
                                stages.normalize = started.elapsed();
                                clean
                            }
                            Err(error) => {
                                tracing::warn!(%error, "normalization failed; returning raw text");
                                asr.clone()
                            }
                        }
                    }
                };
                // Consumed here rather than where `stages` is built: an
                // error or a cancel discards the stages, and taking the
                // load cost there would lose it for the process.
                first = false;
                Ok(Outcome::Transcript(Transcript { asr, text, stages, truncated: job.truncated }))
            }
            // `was_aborted` is what separates a cancellation from a real
            // fault: both arrive as an error from `run`, and folding them
            // together would report a user's own abort as recognition
            // failing.
            Err(_) if session.was_aborted() => Err(Error::Cancelled),
            Err(source) => Err(Error::Recognition { message: source.to_string() }),
        };
        in_flight.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        let _ = job.reply.send(answer);
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

    /// Teardown must abandon queued work rather than run it. Only
    /// checkable with real weights: without them every job is answered
    /// instantly and drain and discard look identical.
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

        let started = std::time::Instant::now();
        drop(engine);
        let elapsed = started.elapsed();
        drop(queued);
        assert!(
            elapsed < Duration::from_secs(3),
            "six queued inferences must be discarded on teardown, not run; took {elapsed:?}"
        );
    }
}
