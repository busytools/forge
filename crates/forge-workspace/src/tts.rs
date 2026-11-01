//! The `[tts]` section: what the speaking role runs, and the read the voice
//! page draws.

use std::path::PathBuf;

use forge_dictate::{ModelFacts, ModelSpec};
use serde::Deserialize;

use crate::dictate::{DictateModel, DictateModelState, DictateRole};

/// The compiled variant the speaking role's catalogue entry names.
pub const SPEAKING_VARIANT: &str = "qwen3-tts-12hz-1.7b-base";

/// The model files a speaking model is made of. The codec lives in the
/// projector, so a pipeline without one cannot render.
pub struct SpeakingModel {
    pub base: ModelSpec,
    pub projector: ModelSpec,
}

impl SpeakingModel {
    /// The compiled entry: Qwen3-TTS 12Hz 1.7B Base, Q8_0, with the projector
    /// that carries its audio codec. Both blobs publish a SHA-256, so both
    /// are verified on arrival.
    ///
    /// The 0.6B checkpoint has no published GGUF; this is the build the
    /// llama.cpp tool documents. No parameter count is claimed because the
    /// Hub listing publishes none.
    pub fn qwen3_tts() -> Self {
        let facts = ModelFacts {
            quant: Some("Q8_0".to_owned()),
            params: None,
            license: Some("Apache-2.0".to_owned()),
            runtime: Some("llama.cpp".to_owned()),
        };
        Self {
            base: ModelSpec {
                file: "Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf".to_owned(),
                url: "https://huggingface.co/ggml-org/Qwen3-TTS-12Hz-1.7B-Base-GGUF/resolve/main/Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf".to_owned(),
                size: 1_847_874_400,
                sha256: Some(
                    "ac7931aeb2e7aad1a6ed6602d353a5679c9d096b18ce8204ac730a8408d572e1".to_owned(),
                ),
                facts: facts.clone(),
            },
            projector: ModelSpec {
                file: "mmproj-Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf".to_owned(),
                url: "https://huggingface.co/ggml-org/Qwen3-TTS-12Hz-1.7B-Base-GGUF/resolve/main/mmproj-Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf".to_owned(),
                size: 446_422_912,
                sha256: Some(
                    "6fd65188839bcd6ecc91b277ad471e22a0edfada4699a0fe82f1165c18cfcce2".to_owned(),
                ),
                facts,
            },
        }
    }
}

/// Deliberately narrower than a model spec, for the same reason
/// [`crate::dictate::DictateSettings`] is: a hand-edited URL, byte length or
/// digest is how a file arrives that nothing can verify.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TtsSettings {
    /// Off unless asked for: turning it on costs a 2.3 GB download the first
    /// time.
    #[serde(default)]
    pub enabled: bool,
    /// Where the model files live. Absent uses the platform cache directory.
    #[serde(default)]
    pub models_dir: Option<String>,
    /// The model the speaking role runs, as the catalogue's variant name.
    /// Set, it wins over every runtime pick and refuses Activate for the
    /// role.
    #[serde(default)]
    pub speaking_model: Option<String>,
    /// Spoken language. Absent autodetects.
    #[serde(default)]
    pub language: Option<String>,
    /// The model's own speaker name. Absent uses its default.
    #[serde(default)]
    pub speaker: Option<String>,
    /// Upper bound on one utterance's text, not one sentence.
    #[serde(default = "default_max_utterance_seconds")]
    pub max_utterance_seconds: u64,
}

fn default_max_utterance_seconds() -> u64 {
    120
}

impl Default for TtsSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            models_dir: None,
            speaking_model: None,
            language: None,
            speaker: None,
            max_utterance_seconds: default_max_utterance_seconds(),
        }
    }
}

impl TtsSettings {
    /// The catalogue entry the speaking role runs.
    pub fn model(&self) -> SpeakingModel {
        SpeakingModel::qwen3_tts()
    }

    /// Where the model files land.
    pub fn models_dir(&self) -> Option<PathBuf> {
        match self.models_dir.as_deref() {
            Some(dir) => Some(crate::config::expand_home(dir)),
            None => dirs::cache_dir().map(|dir| dir.join("forge-tts")),
        }
    }

    /// The two files, resolved under the models directory.
    pub fn files(&self) -> Option<forge_tts::ModelFiles> {
        let dir = self.models_dir()?;
        let model = self.model();
        Some(forge_tts::ModelFiles::from_dir(&dir, &model.base.file, Some(&model.projector.file)))
    }

    /// The role's compiled pin.
    pub(crate) fn pin(&self) -> ModelSpec {
        self.model().base
    }
}

/// The speaking role's model rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TtsSnapshot {
    pub models: Vec<DictateModel>,
}

/// The voice read: whether `[tts]` is on, where the files land, and the
/// speaking model's state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TtsView {
    pub enabled: bool,
    pub snapshot: TtsSnapshot,
    pub models_dir: Option<PathBuf>,
}

impl crate::Workspace {
    /// The voice page's read.
    pub fn voices(&self) -> TtsView {
        let config = self.config_arc();
        let settings = &config.tts;
        let model = settings.model();
        let fetched = settings.files().is_some_and(|files| {
            files.model.is_file() && files.projector.is_some_and(|projector| projector.is_file())
        });
        let state = if fetched { DictateModelState::Ready } else { DictateModelState::Pending };
        let snapshot = TtsSnapshot {
            models: if settings.enabled {
                vec![DictateModel { role: DictateRole::Speaking, file: model.base.file, state }]
            } else {
                Vec::new()
            },
        };
        TtsView { enabled: settings.enabled, snapshot, models_dir: settings.models_dir() }
    }

    /// The speaking role's model rows alone.
    pub fn voice_models(&self) -> TtsSnapshot {
        self.voices().snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::TtsSettings;

    #[test]
    fn an_absent_tts_section_is_off() {
        let settings: TtsSettings = toml::from_str("").expect("parses");
        assert!(!settings.enabled);
        assert_eq!(settings.max_utterance_seconds, 120);
    }

    #[test]
    fn an_unknown_tts_key_fails_the_load() {
        let error = toml::from_str::<TtsSettings>("speek = \"x\"").expect_err("refused");
        assert!(error.to_string().contains("speek"));
    }
}
