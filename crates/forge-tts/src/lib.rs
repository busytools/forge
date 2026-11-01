//! Speech synthesis: text in, audio chunks out.
//!
//! A leaf crate. It depends on no other forge crate and knows nothing about
//! a seat, a tool or a wire: a host hands [`SpeakRequest`]s to an [`Engine`]
//! and reads [`Chunk`]s off the returned channel. Which engine renders, and
//! when a chunk reaches the speaker, stays with the host.

mod engine;
mod error;
mod model;

pub use engine::QwenTts;
pub use error::Error;
pub use model::ModelFiles;

/// One utterance to render, with the model's own voice settings.
#[derive(Debug, Clone)]
pub struct SpeakRequest {
    pub text: String,
    pub language: Option<String>,
    pub speaker: Option<String>,
}

/// A rendered run of audio, in arrival order.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub seq: u32,
    pub sample_rate: u32,
    pub pcm: Vec<i16>,
}

/// Text-to-speech rendering. Channels rather than `&mut self`, so a caller
/// can hold one engine and serve every queue without a mutex on the render
/// path.
pub trait Engine: Send + Sync {
    /// Render one utterance. Chunks arrive in order and the channel closes
    /// when the render finishes or fails; the host may stop reading at any
    /// time.
    fn speak(&self, request: SpeakRequest) -> tokio::sync::mpsc::Receiver<Result<Chunk, Error>>;
}

#[cfg(test)]
mod tests {
    use super::{Error, SpeakRequest};

    #[test]
    fn a_speak_request_carries_text_and_optional_voice_settings() {
        let request = SpeakRequest {
            text: "hello".to_owned(),
            language: None,
            speaker: Some("default".to_owned()),
        };
        assert_eq!(request.text, "hello");
        assert_eq!(request.speaker.as_deref(), Some("default"));
    }

    #[test]
    fn an_error_names_its_stage() {
        let error = Error::ModelMissing("/models/qwen.gguf".to_owned());
        assert!(error.to_string().contains("qwen.gguf"));
    }

    /// This crate is a leaf, and depending on a forge-* crate is the one
    /// change that would quietly end that: such an edge closes no cycle, so
    /// the workspace still builds.
    #[test]
    fn depends_on_no_forge_crate() {
        let output = std::process::Command::new(option_env!("CARGO").unwrap_or("cargo"))
            .args([
                "metadata",
                "--no-deps",
                "--offline",
                "--format-version",
                "1",
                "--manifest-path",
                concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            ])
            .output()
            .expect("cargo metadata must run: a guard that cannot run is not a guard");
        assert!(
            output.status.success(),
            "cargo metadata failed, so the invariant is unchecked: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let metadata: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("cargo metadata emits json");
        let package = metadata["packages"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["name"] == "forge-tts")
            .expect("forge-tts must appear in its own metadata");

        let forge: Vec<&str> = package["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|dep| dep["name"].as_str())
            .filter(|name| name.starts_with("forge-"))
            .collect();
        assert!(forge.is_empty(), "forge-tts must depend on no forge-* crate, found {forge:?}");
    }
}
