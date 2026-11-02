//! The in-process Kokoro-82M engine.

use std::sync::{Mutex, mpsc};

use tokio::sync::mpsc as tokio_mpsc;

use crate::model::ModelFiles;
use crate::{Chunk, Engine, Error, SpeakRequest};

/// The wire's sample rate: 24 kHz mono 16-bit. Kokoro renders at this rate
/// natively; anything else is resampled once, here.
pub const SAMPLE_RATE: u32 = 24_000;

/// The voice an utterance renders in when the request names none.
pub const DEFAULT_VOICE: &str = "af_heart";

/// A loaded Kokoro-82M pipeline. One render thread per engine, so a second
/// utterance queues behind the first; the thread owns the ONNX session and
/// the runtime the engine's async API needs.
///
/// Kokoro renders a whole clip, so one `speak` answers one chunk.
pub struct KokoroTts {
    jobs: Option<tokio_mpsc::UnboundedSender<Job>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct Job {
    request: SpeakRequest,
    reply: tokio_mpsc::Sender<Result<Chunk, Error>>,
}

/// The installed voices a request is checked against.
///
/// The engine's own lookup falls back to a lone loaded voice when the name
/// is unknown, which is the silent default the wire must not have.
pub fn select_voice(requested: Option<&str>, installed: &[String]) -> Result<String, Error> {
    let id = requested.unwrap_or(DEFAULT_VOICE);
    if installed.iter().any(|voice| voice == id) {
        Ok(id.to_owned())
    } else {
        Err(Error::VoiceMissing(id.to_owned()))
    }
}

impl KokoroTts {
    /// Load the model and every voice in `files.voices`, on a thread of their
    /// own. A caller may warm it ahead of the first `say`, and a failed warm
    /// is retried by the next one. Every declared digest is checked before
    /// the load.
    pub fn load(files: &ModelFiles) -> Result<Self, Error> {
        files.verify()?;
        let installed = files.installed_voices()?;
        let (jobs, mut queue) = tokio_mpsc::unbounded_channel::<Job>();
        let (ready, loaded) = mpsc::channel();
        let model = files.model.clone();
        let voices = files.voices.clone();
        let worker_installed = installed.clone();
        let worker = std::thread::Builder::new()
            .name("forge-tts".to_owned())
            .spawn(move || {
                let runtime =
                    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let _ = ready.send(Err(Error::Load(error.to_string())));
                            return;
                        }
                    };
                let engine = match runtime.block_on(kokoro_en::KokoroTts::new(&model, &voices)) {
                    Ok(engine) => engine,
                    Err(error) => {
                        let _ = ready.send(Err(Error::Load(error.to_string())));
                        return;
                    }
                };
                let mut renderer = Kokoro { runtime, engine };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while let Some(job) = queue.blocking_recv() {
                    // A send failure is the host stopping playback, so this
                    // render's result has nowhere to go.
                    let _ =
                        job.reply.blocking_send(chunk_for(&mut renderer, &worker_installed, &job.request));
                }
            })
            .map_err(|error| Error::Load(error.to_string()))?;

        match loaded.recv() {
            Ok(Ok(())) => {
                Ok(Self { jobs: Some(jobs), worker: Mutex::new(Some(worker)) })
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Load("the render thread stopped while loading".to_owned())),
        }
    }
}

impl Drop for KokoroTts {
    fn drop(&mut self) {
        // Close the queue first, so the thread ends its loop, drops the
        // session and only then lets the join return.
        drop(self.jobs.take());
        let worker = self.worker.get_mut().ok().and_then(Option::take);
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

impl Engine for KokoroTts {
    fn speak(&self, request: SpeakRequest) -> tokio_mpsc::Receiver<Result<Chunk, Error>> {
        let Some(jobs) = self.jobs.as_ref() else {
            return refused();
        };
        let (reply, chunks) = tokio_mpsc::channel(2);
        if jobs.send(Job { request, reply }).is_err() {
            return refused();
        }
        chunks
    }
}

/// A channel carrying one failure, for a render thread that is gone.
fn refused() -> tokio_mpsc::Receiver<Result<Chunk, Error>> {
    let (reply, chunks) = tokio_mpsc::channel(1);
    let _ = reply.try_send(Err(Error::Render(
        "the render thread is no longer running".to_owned(),
    )));
    chunks
}

/// What the engine's inference looks like to the wire conversion, so a test
/// can drive the conversion with no model load.
trait Renderer: Send {
    fn native_rate(&self) -> u32;
    fn render(&mut self, text: &str, voice: &str) -> Result<Vec<f32>, Error>;
}

/// One render: the voice the request names, the model's samples, and the
/// wire's own shape.
fn chunk_for(
    renderer: &mut dyn Renderer,
    installed: &[String],
    request: &SpeakRequest,
) -> Result<Chunk, Error> {
    let voice = select_voice(request.voice.as_deref(), installed)?;
    let samples = renderer.render(&request.text, &voice)?;
    let samples = resample(&samples, renderer.native_rate(), SAMPLE_RATE);
    Ok(Chunk { seq: 0, sample_rate: SAMPLE_RATE, pcm: pcm16(&samples) })
}

/// Linear resampling to the wire rate; identity when the rates match.
fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = f64::from(from) / f64::from(to);
    let length = ((samples.len() as f64) * f64::from(to) / f64::from(from)).round() as usize;
    let last = samples.len() - 1;
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let position = index as f64 * ratio;
        let left = position.floor() as usize;
        let fraction = (position - left as f64) as f32;
        let a = samples[left.min(last)];
        let b = samples[(left + 1).min(last)];
        out.push(a + (b - a) * fraction);
    }
    out
}

/// The wire's sample: clamp to [-1, 1], then the signed 16-bit range.
fn pcm16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|sample| {
            let clamped = sample.clamp(-1.0, 1.0);
            (clamped * f32::from(i16::MAX)) as i16
        })
        .collect()
}

/// The loaded engine, as the wire conversion sees it.
struct Kokoro {
    runtime: tokio::runtime::Runtime,
    engine: kokoro_en::KokoroTts,
}

impl Renderer for Kokoro {
    fn native_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn render(&mut self, text: &str, voice: &str) -> Result<Vec<f32>, Error> {
        self.runtime
            .block_on(self.engine.synth(text, voice))
            .map(|(audio, _)| audio)
            .map_err(|error| Error::Render(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_VOICE, Renderer, SAMPLE_RATE, chunk_for, select_voice};
    use crate::{Engine as _, Error, SpeakRequest};

    /// A renderer that records the voice it was asked for and answers with
    /// `samples` at `rate`.
    struct Stub {
        rate: u32,
        samples: Vec<f32>,
        spoken: Vec<String>,
    }

    impl Stub {
        fn new(rate: u32, samples: Vec<f32>) -> Self {
            Self { rate, samples, spoken: Vec::new() }
        }
    }

    impl Renderer for Stub {
        fn native_rate(&self) -> u32 {
            self.rate
        }

        fn render(&mut self, _text: &str, voice: &str) -> Result<Vec<f32>, Error> {
            self.spoken.push(voice.to_owned());
            Ok(self.samples.clone())
        }
    }

    fn installed() -> Vec<String> {
        vec!["af_heart".to_owned(), "bf_emma".to_owned()]
    }

    #[test]
    fn a_chosen_voice_is_the_one_the_render_uses() {
        let mut stub = Stub::new(SAMPLE_RATE, vec![0.0; 8]);
        let request = SpeakRequest { text: "hello".to_owned(), voice: Some("bf_emma".to_owned()) };
        chunk_for(&mut stub, &installed(), &request).expect("the render lands");
        assert_eq!(stub.spoken, vec!["bf_emma".to_owned()]);
    }

    #[test]
    fn no_chosen_voice_renders_in_the_default() {
        let mut stub = Stub::new(SAMPLE_RATE, vec![0.0; 8]);
        let request = SpeakRequest { text: "hello".to_owned(), voice: None };
        chunk_for(&mut stub, &installed(), &request).expect("the render lands");
        assert_eq!(stub.spoken, vec![DEFAULT_VOICE.to_owned()]);
    }

    #[test]
    fn an_unknown_voice_is_refused_by_name() {
        let error = select_voice(Some("zf_nobody"), &installed()).expect_err("refused");
        assert_eq!(error, Error::VoiceMissing("zf_nobody".to_owned()));
        assert!(error.to_string().contains("zf_nobody"), "got: {error}");
    }

    #[test]
    fn a_default_voice_that_is_not_installed_is_refused_rather_than_replaced() {
        let error = select_voice(None, &["bf_emma".to_owned()]).expect_err("refused");
        assert_eq!(error, Error::VoiceMissing(DEFAULT_VOICE.to_owned()));
    }

    /// The model's own rate never reaches the wire: a 16 kHz render arrives
    /// at 24 kHz, mono, as signed 16-bit samples.
    #[test]
    fn the_wire_chunk_is_24k_mono_sixteen_bit_whatever_the_model_rate() {
        let mut stub = Stub::new(16_000, vec![0.5; 16]);
        let request = SpeakRequest { text: "hello".to_owned(), voice: None };
        let chunk = chunk_for(&mut stub, &installed(), &request).expect("the render lands");
        assert_eq!(chunk.sample_rate, SAMPLE_RATE);
        assert_eq!(chunk.pcm.len(), 24, "1 ms at 16 kHz is 1 ms at 24 kHz");
        assert!(chunk.pcm.iter().all(|sample| *sample == 16_383), "0.5 scales to half the range");
    }

    #[test]
    fn a_sample_past_full_scale_is_clamped_not_wrapped() {
        let mut stub = Stub::new(SAMPLE_RATE, vec![2.0, -2.0]);
        let request = SpeakRequest { text: "loud".to_owned(), voice: None };
        let chunk = chunk_for(&mut stub, &installed(), &request).expect("the render lands");
        assert_eq!(chunk.pcm, vec![i16::MAX, -i16::MAX]);
    }

    /// Loads the real weights, so it runs only where they are on disk:
    /// `FORGE_TTS_MODEL_DIR` must hold `model_quantized.onnx` and a
    /// `voices/` directory with at least the default voice.
    #[tokio::test]
    #[ignore = "loads the real model"]
    async fn the_engine_speaks_a_short_sentence() {
        let dir = std::path::PathBuf::from(
            std::env::var("FORGE_TTS_MODEL_DIR").expect("FORGE_TTS_MODEL_DIR"),
        );
        let files = crate::ModelFiles::new(
            dir.join("model_quantized.onnx"),
            dir.join("voices"),
        );
        let engine = crate::KokoroTts::load(&files).expect("loads");
        let mut chunks = engine.speak(SpeakRequest {
            text: "Ready when you are.".to_owned(),
            voice: None,
        });
        let first = chunks.recv().await.expect("a chunk").expect("a render");
        assert_eq!(first.sample_rate, SAMPLE_RATE);
        assert!(!first.pcm.is_empty());
    }
}
