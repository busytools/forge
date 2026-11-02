//! The in-process Qwen3-TTS engine.

mod ffi;

use tokio::sync::mpsc;

use crate::model::ModelFiles;
use crate::{Chunk, Engine, Error, SpeakRequest};

/// Initialise llama.cpp's backend registry once, from the boot path, so no
/// later caller can race the load. Idempotent.
pub fn init_backend() {
    ffi::init_backend();
}

/// A loaded Qwen3-TTS pipeline. One render thread per engine, so a second
/// utterance queues behind the first.
///
/// The thread is joined on drop: a detached thread still holding buffers at
/// exit trips ggml's Metal residency-set assert.
pub struct QwenTts {
    jobs: Option<mpsc::UnboundedSender<Job>>,
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct Job {
    request: SpeakRequest,
    reply: mpsc::Sender<Result<Chunk, Error>>,
}

impl QwenTts {
    /// Load the backbone and its codec projector once, on a thread of their
    /// own; the first `say` is the first load. Both files are checked
    /// against the digests their entries declare before the load.
    pub fn load(files: &ModelFiles) -> Result<Self, Error> {
        let Some(projector) = files.projector.as_ref() else {
            return Err(Error::ModelMissing("a TTS model needs its projector".to_owned()));
        };
        for path in [&files.model, projector] {
            if !path.is_file() {
                return Err(Error::ModelMissing(path.display().to_string()));
            }
        }
        files.verify()?;

        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (ready, loaded) = std::sync::mpsc::channel();
        let (model_path, projector_path) = (files.model.clone(), projector.clone());
        let worker = std::thread::Builder::new()
            .name("forge-tts".to_owned())
            .spawn(move || match ffi::Core::load(&model_path, &projector_path) {
                Ok(mut core) => {
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    while let Some(job) = queue.blocking_recv() {
                        if let Err(error) = core.render(&job.request, &job.reply) {
                            // A send failure is the host stopping playback,
                            // so the error has nowhere to go either.
                            let _ = job.reply.blocking_send(Err(error));
                        }
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })
            .map_err(|error| Error::Load(error.to_string()))?;

        match loaded.recv() {
            Ok(Ok(())) => {
                Ok(Self { jobs: Some(jobs), worker: std::sync::Mutex::new(Some(worker)) })
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Load("the render thread stopped while loading".to_owned())),
        }
    }
}

impl Drop for QwenTts {
    fn drop(&mut self) {
        // Close the queue first, so the thread ends its loop, drops the core
        // and only then lets the join return.
        drop(self.jobs.take());
        let worker = self.worker.get_mut().ok().and_then(Option::take);
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

impl Engine for QwenTts {
    fn speak(&self, request: SpeakRequest) -> mpsc::Receiver<Result<Chunk, Error>> {
        let Some(jobs) = self.jobs.as_ref() else {
            return refused();
        };
        let (reply, chunks) = mpsc::channel(2);
        if jobs.send(Job { request, reply }).is_err() {
            return refused();
        }
        chunks
    }
}

/// A channel carrying one failure, for a render thread that is gone.
fn refused() -> mpsc::Receiver<Result<Chunk, Error>> {
    let (reply, chunks) = mpsc::channel(1);
    let _ = reply.try_send(Err(Error::Render("the render thread is no longer running".to_owned())));
    chunks
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::QwenTts;
    use crate::model::ModelFiles;
    use crate::{Engine, SpeakRequest};

    /// Loads the real weights, so it runs only where they are on disk.
    #[tokio::test]
    #[ignore = "loads the real model"]
    async fn the_engine_speaks_a_short_sentence() {
        let dir = PathBuf::from(std::env::var("FORGE_TTS_MODEL_DIR").expect("FORGE_TTS_MODEL_DIR"));
        let files = ModelFiles::from_dir(
            &dir,
            "Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf",
            Some("mmproj-Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf"),
        );
        let engine = QwenTts::load(&files).expect("loads");
        let mut chunks = engine.speak(SpeakRequest {
            text: "Ready when you are.".to_owned(),
            language: Some("en".to_owned()),
        });
        let first = chunks.recv().await.expect("a chunk").expect("a render");
        assert!(first.sample_rate > 0);
        assert!(!first.pcm.is_empty());
        // The audio arrives as it renders rather than in one clip at the end.
        let mut rest = 0;
        while let Some(next) = chunks.recv().await {
            next.expect("a render");
            rest += 1;
        }
        assert!(rest > 0, "a sentence streams more than one chunk");
    }
}
