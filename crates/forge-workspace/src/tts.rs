//! The `[tts]` section: what the speaking role runs, and the read the models
//! page draws.

use std::path::PathBuf;

use forge_dictate::{ModelFacts, ModelSpec};
use serde::Deserialize;

use crate::dictate::{DictateModel, DictateModelState, DictateRole};

/// The model files a speaking model is made of. The codec lives in the
/// projector, so a pipeline without one cannot render.
pub struct SpeakingModel {
    pub base: ModelSpec,
    pub projector: ModelSpec,
}

impl SpeakingModel {
    /// The compiled entry: Qwen3-TTS 12Hz 1.7B Base, Q8_0, with the projector
    /// that carries its audio codec. Both blobs declare a SHA-256, and the
    /// engine checks both before it loads them.
    ///
    /// ggml-org publishes no 0.6B GGUF; this is the build the llama.cpp
    /// tool documents. No parameter count is claimed because the Hub
    /// listing publishes none.
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
    /// Off unless asked for. Turning it on needs the two model files placed
    /// under `models_dir` by hand, about 2.3 GB together.
    #[serde(default)]
    pub enabled: bool,
    /// Where the model files live. Absent uses the platform cache directory.
    #[serde(default)]
    pub models_dir: Option<String>,
    /// Spoken language. Absent leaves the pipeline's own default, English.
    #[serde(default)]
    pub language: Option<String>,
    /// Upper bound on one utterance's text, as a character estimate at a
    /// rough speaking pace, not one sentence.
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
            language: None,
            max_utterance_seconds: default_max_utterance_seconds(),
        }
    }
}

impl TtsSettings {
    /// The compiled model the speaking role runs. There is no key that
    /// selects another: the two files are placed by hand and the engine
    /// loads them.
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

    /// The two files, resolved under the models directory, with the digests
    /// their entries declare so a load can verify them.
    pub fn files(&self) -> Option<forge_tts::ModelFiles> {
        let dir = self.models_dir()?;
        let model = self.model();
        Some(
            forge_tts::ModelFiles::from_dir(&dir, &model.base.file, Some(&model.projector.file))
                .with_digests(model.base.sha256.clone(), model.projector.sha256.clone()),
        )
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
    /// The models page's read.
    pub fn voices(&self) -> TtsView {
        let config = self.config_arc();
        let settings = &config.tts;
        let model = settings.model();
        let fetched = settings.files().is_some_and(|files| {
            files.model.is_file() && files.projector.is_some_and(|projector| projector.is_file())
        });
        let state = if fetched { DictateModelState::Ready } else { DictateModelState::Pending };
        // The running system's flag, not a saved value the restart has not
        // taken: the tool gate and the registered set read the same one.
        let enabled = self.voice_enabled();
        let snapshot = TtsSnapshot {
            models: if enabled {
                vec![DictateModel { role: DictateRole::Speaking, file: model.base.file, state }]
            } else {
                Vec::new()
            },
        };
        TtsView { enabled, snapshot, models_dir: settings.models_dir() }
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

    /// A save marked restart cannot move the read away from the running
    /// service: the home read and the registered set answer the same flag
    /// until the restart takes it.
    #[tokio::test]
    async fn a_saved_tts_off_does_not_move_the_read_before_the_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.tts.enabled = true;
        let (workspace, _updates) =
            crate::Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
                .expect("the stub builds");
        assert!(workspace.voice_enabled(), "the fixture starts on");
        let _service = workspace.voice_service();

        // The save lands, and the key is marked restart.
        let mut saved = crate::config::LoadedConfig::empty_for_test();
        saved.tts.enabled = false;
        *workspace.config.write() = std::sync::Arc::new(saved);

        assert!(workspace.voice_enabled(), "the running service still answers on");
        assert!(workspace.voices().enabled, "the home read follows the service");
    }
}
