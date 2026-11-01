//! The in-process Qwen3-TTS engine.

mod ffi;

use std::path::Path;

use tokio::sync::mpsc;

use crate::{Chunk, Engine, Error, SpeakRequest};

/// A loaded Qwen3-TTS pipeline. One render thread per engine, so a second
/// utterance queues behind the first and the model is never shared mid-render.
pub struct QwenTts {
    jobs: mpsc::UnboundedSender<Job>,
}

struct Job {
    request: SpeakRequest,
    reply: mpsc::Sender<Result<Chunk, Error>>,
}

impl QwenTts {
    /// Load the backbone and its codec projector once, on a thread of their
    /// own, so the first utterance is not the first load.
    pub fn load(model: &Path, projector: Option<&Path>) -> Result<Self, Error> {
        let Some(projector) = projector else {
            return Err(Error::ModelMissing("a TTS model needs its projector".to_owned()));
        };
        for path in [model, projector] {
            if !path.is_file() {
                return Err(Error::ModelMissing(path.display().to_string()));
            }
        }

        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        let (ready, loaded) = std::sync::mpsc::channel();
        let (model_path, projector_path) = (model.to_path_buf(), projector.to_path_buf());
        std::thread::Builder::new()
            .name("forge-tts".to_owned())
            .spawn(move || match ffi::Core::load(&model_path, &projector_path) {
                Ok(mut core) => {
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    while let Some(job) = queue.blocking_recv() {
                        // A send failure is the host stopping playback, so
                        // this render's result has nowhere to go.
                        let _ = job.reply.blocking_send(core.render(&job.request));
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })
            .map_err(|error| Error::Load(error.to_string()))?;

        match loaded.recv() {
            Ok(Ok(())) => Ok(Self { jobs }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Load("the render thread stopped while loading".to_owned())),
        }
    }
}

impl Engine for QwenTts {
    fn speak(&self, request: SpeakRequest) -> mpsc::Receiver<Result<Chunk, Error>> {
        let (reply, chunks) = mpsc::channel(2);
        if let Err(error) = self.jobs.send(Job { request, reply }) {
            let _ = error
                .0
                .reply
                .try_send(Err(Error::Render("the render thread is no longer running".to_owned())));
        }
        chunks
    }
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
        let engine = QwenTts::load(&files.model, files.projector.as_deref()).expect("loads");
        let mut chunks = engine.speak(SpeakRequest {
            text: "Ready when you are.".to_owned(),
            language: Some("en".to_owned()),
            speaker: None,
        });
        let first = chunks.recv().await.expect("a chunk").expect("a render");
        assert!(first.sample_rate > 0);
        assert!(!first.pcm.is_empty());
    }
}
