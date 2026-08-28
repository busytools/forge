//! The transcription engine and the tickets it hands back.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use transcribe_cpp::{CancelToken, Feature, Model, RunOptions};

use crate::audio::{AudioSource, SAMPLE_RATE};
use crate::{Config, Error};

/// Native diagnostics are routed once per process, before any model is
/// loaded. Unrouted, one load plus one inference writes 122 lines
/// straight to stderr, which corrupts any full-screen terminal the host
/// happens to be drawing. Routing rather than silencing keeps them
/// reachable through the `log` facade instead of destroying them.
static ROUTE_NATIVE_LOGS: Once = Once::new();

/// Where the time went in one transcription.
///
/// Part of the normal result rather than a test hook: which stage moved
/// is the only thing that distinguishes "our code got slower" from "the
/// model changed", and a caller rendering diagnostics wants it.
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
    },
}

/// One queued transcription.
struct Job {
    pcm: Vec<f32>,
    resample: Duration,
    cancel: CancelToken,
    reply: Sender<Result<Outcome, Error>>,
}

/// Loads the models and runs transcriptions, one at a time.
///
/// Serialized deliberately rather than for convenience: the recognition
/// session needs `&mut self` to run, so one set of weights means one
/// caller at a time whatever the API looks like.
pub struct Engine {
    /// `None` only while dropping, which is what ends the worker's loop.
    jobs: Option<Sender<Job>>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Label of whoever holds the microphone, if anyone.
    holder: Arc<Mutex<Option<String>>>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Dropping the queue ends the worker's loop; JOINING it is what
        // makes the weights release before the process tears down the
        // native backend. Without the join the two race, and ggml's
        // Metal teardown aborts the process with
        // `GGML_ASSERT([rsets->data count] == 0)` - after a clean run,
        // so it reads as a crash on quit with no connection to dictation.
        self.jobs.take();
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
        ROUTE_NATIVE_LOGS.call_once(transcribe_cpp::init_logging);

        let dir = crate::fetch::models_dir(&cfg)?;
        let asr_path = dir.join(&cfg.asr_model.file);
        let (jobs, queue) = channel();

        let handle = std::thread::Builder::new()
            .name("forge-dictate".into())
            .spawn(move || worker(&asr_path, &cfg, &queue))
            .map_err(|source| Error::Io { path: dir, source })?;

        Ok(Arc::new(Engine {
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
        // path and for sources with no hardware behind them, not because
        // the runtime wants it.
        let started = Instant::now();
        let mut pcm = Vec::new();
        while let Some(chunk) = source.next_chunk() {
            pcm.extend_from_slice(&chunk);
        }
        let resample = started.elapsed();

        let (reply, answer) = channel();
        let cancel = CancelToken::new();
        self.jobs
            .as_ref()
            .ok_or(Error::EngineStopped)?
            .send(Job { pcm, resample, cancel: cancel.clone(), reply })
            .map_err(|_| Error::EngineStopped)?;
        Ok(Ticket { answer, cancel })
    }

    /// Take the microphone, labelling the holder so a competing caller
    /// can be told who has it.
    ///
    /// Exclusive WITHIN THIS PROCESS ONLY. A second process running its
    /// own engine is not arbitrated; the operating system decides, and
    /// both may record.
    pub fn try_capture(&self, holder: impl Into<String>) -> Result<Capture, Busy> {
        let mut lock = self.holder.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(held) = lock.as_deref() {
            return Err(Busy { holder: held.to_owned() });
        }
        *lock = Some(holder.into());
        drop(lock);
        Ok(Capture { holder: Arc::clone(&self.holder) })
    }

    /// Loudest sample in `pcm`, in dBFS.
    fn peak_dbfs(pcm: &[f32]) -> f32 {
        let peak = pcm.iter().fold(0.0f32, |worst, s| worst.max(s.abs()));
        if peak <= 0.0 { f32::NEG_INFINITY } else { 20.0 * peak.log10() }
    }
}

/// Somebody else holds the microphone.
#[derive(Debug, Clone, thiserror::Error)]
#[error("the microphone is held by {holder}")]
pub struct Busy {
    /// Label the current holder passed to [`Engine::try_capture`].
    pub holder: String,
}

/// Holds the microphone until dropped.
///
/// Release is tied to the value rather than to a method so a caller that
/// panics mid-capture cannot wedge the input for everyone else in the
/// process.
pub struct Capture {
    holder: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.holder.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("Capture").field("holder", &held.as_deref()).finish_non_exhaustive()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
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

    /// Give up on the result. Dropping a ticket does the same thing.
    ///
    /// The run really is aborted rather than merely ignored: the
    /// recognition family in use reports `Feature::Cancellation`, so the
    /// worker stops instead of finishing work nobody wants.
    pub fn abandon(self) {
        drop(self);
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Owns the weights and drains the queue.
fn worker(asr_path: &std::path::Path, cfg: &Config, queue: &Receiver<Job>) {
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

    let cancellable = model.supports(Feature::Cancellation);
    let mut options = RunOptions::default();
    options.language.clone_from(&cfg.language);
    let mut first = true;

    while let Ok(job) = queue.recv() {
        let samples = job.pcm.len() as u64;
        let mut stages = Stages {
            model_load: if first { model_load } else { Duration::ZERO },
            resample: job.resample,
            audio: Duration::from_micros(
                samples.saturating_mul(1_000_000) / u64::from(SAMPLE_RATE),
            ),
            ..Stages::default()
        };
        first = false;

        let peak = Engine::peak_dbfs(&job.pcm);
        if peak < cfg.silence_floor {
            let _ = job.reply.send(Ok(Outcome::NoAudio { peak }));
            continue;
        }

        if cancellable {
            session.set_cancel_token(&job.cancel);
        }
        let answer = match session.run(&job.pcm, &options) {
            Ok(out) => {
                stages.mel = Duration::from_secs_f64(f64::from(out.timings.mel_ms) / 1000.0);
                stages.encode = Duration::from_secs_f64(f64::from(out.timings.encode_ms) / 1000.0);
                stages.decode = Duration::from_secs_f64(f64::from(out.timings.decode_ms) / 1000.0);
                let text = out.text.trim().to_owned();
                Ok(Outcome::Transcript(Transcript { asr: text.clone(), text, stages }))
            }
            Err(source) => Err(Error::Recognition { message: source.to_string() }),
        };
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

    #[test]
    fn silence_is_not_an_empty_transcript() {
        let silent = Engine::peak_dbfs(&[0.0; 32]);
        assert!(
            silent.is_infinite() && silent.is_sign_negative(),
            "digital silence must read as no signal at all, not as a quiet one, got {silent}"
        );
        // Half scale is -6 dBFS, comfortably above the -50 default.
        assert!(
            Engine::peak_dbfs(&[0.0, 0.5, -0.25]) > -50.0,
            "audible speech must sit above the default floor, or every capture reads as silence"
        );
    }
}

/// Real recognition against the real weights.
///
/// Ignored by default because it needs a 1.5 GB model that CI does not
/// have. It is not decoration: it is the only test that proves the crate
/// transcribes rather than merely compiles.
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
}
