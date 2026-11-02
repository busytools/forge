//! Speech synthesis: text in, audio chunks out.
//!
//! A leaf crate. It depends on no other forge crate and knows nothing about
//! a seat, a tool or a wire: a host hands [`SpeakRequest`]s to an [`Engine`]
//! and reads [`Chunk`]s off the returned channel. Which engine renders, and
//! when a chunk reaches the speaker, stays with the host.

pub mod bench;
mod engine;
mod error;
mod model;
mod split;

pub use engine::{DEFAULT_VOICE, KokoroTts, SAMPLE_RATE, select_voice};
pub use error::Error;
pub use model::ModelFiles;
pub use split::{sentences, within_bound};

/// One utterance to render, with the voice it renders in.
#[derive(Debug, Clone)]
pub struct SpeakRequest {
    pub text: String,
    /// The voice id for this utterance. `None` uses [`DEFAULT_VOICE`]; an id
    /// that is not installed is refused rather than replaced by one.
    pub voice: Option<String>,
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
    use super::Error;

    #[test]
    fn an_error_names_its_stage() {
        let error = Error::ModelMissing("/models/kokoro.onnx".to_owned());
        assert!(error.to_string().contains("kokoro.onnx"));
    }

    /// The engine swap left nothing behind: the crate's manifest carries no
    /// llama.cpp dependency and the FFI module it lived in is gone.
    #[test]
    fn the_qwen_engine_is_gone() {
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("the crate's own manifest reads");
        assert!(!manifest.contains("llama"), "forge-tts still depends on llama.cpp:\n{manifest}");
        assert!(
            !std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/engine/ffi.rs"))
                .exists(),
            "the Qwen FFI module is still in the crate"
        );
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
