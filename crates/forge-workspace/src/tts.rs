//! The `[tts]` section: what the speaking role runs, the voice it speaks
//! in, and the read the models and settings pages draw.

use std::path::{Path, PathBuf};

use forge_dictate::catalogue::{CatalogueEntry, Download, EntryKind, License};
use forge_dictate::{ModelFacts, ModelSpec};
use serde::Deserialize;

use crate::dictate::{DictateModelState, DictateRole};
use crate::install::{ActiveChoice, ActiveFrom, InstalledModel};

/// The variant `[tts] speaking_model` names. It is forge's own entry: the
/// speech feed carries no speaking kind, so nothing outside this repo can
/// add one.
pub const SPEAKING_VARIANT: &str = "qwen3-tts-12hz-1.7b-base";

/// The model files a speaking model is made of. The codec lives in the
/// projector, so a pipeline without one cannot render.
#[derive(Debug)]
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

    /// The compiled model a variant names, when it is the built-in one.
    pub fn builtin(variant: &str) -> Option<Self> {
        (variant == SPEAKING_VARIANT).then(Self::qwen3_tts)
    }

    /// The catalogue entry the built-in variant is: the shape the models
    /// page already draws for every role, with both files and their digests.
    pub fn entry() -> CatalogueEntry {
        let model = Self::qwen3_tts();
        let download = |spec: &ModelSpec| Download {
            quant: "Q8_0".to_owned(),
            filename: spec.file.clone(),
            size_bytes: spec.size,
            sha256: spec.sha256.clone(),
            url: Some(spec.url.clone()),
        };
        CatalogueEntry {
            variant: SPEAKING_VARIANT.to_owned(),
            display_name: "Qwen3-TTS 1.7B Base".to_owned(),
            family: "qwen3-tts".to_owned(),
            params: None,
            license: Some(License {
                spdx: "apache-2.0".to_owned(),
                display: "Apache-2.0".to_owned(),
            }),
            languages: vec!["en".to_owned()],
            capabilities: forge_dictate::catalogue::Capabilities::default(),
            headline_benchmark: None,
            published_repo: Some("ggml-org/Qwen3-TTS-12Hz-1.7B-Base-GGUF".to_owned()),
            downloads: vec![download(&model.base), download(&model.projector)],
            speed_benchmarks: Vec::new(),
            accuracy_benchmarks: Vec::new(),
            kind: EntryKind::Speaking,
            download_count: None,
        }
    }

    /// The two files under `dir`, carrying the digests the entries declare
    /// so a load verifies them.
    pub fn files(&self, dir: &Path) -> forge_tts::ModelFiles {
        forge_tts::ModelFiles::from_dir(dir, &self.base.file, Some(&self.projector.file))
            .with_digests(self.base.sha256.clone(), self.projector.sha256.clone())
    }
}

/// The speaking role's model, resolved: what it runs and where the choice
/// came from.
#[derive(Debug)]
pub(crate) struct ResolvedSpeaking {
    pub model: SpeakingModel,
    pub variant: String,
    pub from: ActiveFrom,
    /// RFC 3339, when a runtime pick chose it.
    pub at: Option<String>,
}

/// Resolve the speaking role the way the dictation roles resolve: the
/// config key, else the runtime pick, else the compiled pin.
///
/// **A key naming a variant nothing carries is refused by name**, so a typo
/// is the read's own words rather than a silent default. A pick whose
/// record is gone falls to the pin with a warning, as the dictation roles
/// do: forge's own bookkeeping must not refuse the boot.
pub(crate) fn resolve_speaking(
    configured: Option<&str>,
    installed: &[InstalledModel],
    pick: Option<ActiveChoice>,
) -> Result<ResolvedSpeaking, String> {
    if let Some(variant) = configured {
        let Some(model) = SpeakingModel::builtin(variant) else {
            return Err(format!(
                "[tts] speaking_model names {variant}, which is neither an installed model nor a \
                 catalogue entry"
            ));
        };
        return Ok(ResolvedSpeaking {
            model,
            variant: variant.to_owned(),
            from: ActiveFrom::Config {
                key: "speaking_model".to_owned(),
                variant: variant.to_owned(),
            },
            at: None,
        });
    }
    if let Some(choice) = pick {
        let recorded = installed.iter().any(|model| model.variant == choice.variant);
        if let Some(model) = SpeakingModel::builtin(&choice.variant).filter(|_| recorded) {
            return Ok(ResolvedSpeaking {
                model,
                variant: choice.variant.clone(),
                from: ActiveFrom::Installed { variant: choice.variant },
                at: Some(choice.at),
            });
        }
        // A pick whose record is gone falls to the pin rather than refusing
        // the boot: the pick is forge's own bookkeeping, and the removal
        // that strands one clears it.
        tracing::warn!(
            event_name = "tts_pick_without_a_record",
            variant = %choice.variant,
            "the speaking pick names a model no record carries; the compiled pin runs the role"
        );
    }
    Ok(ResolvedSpeaking {
        model: SpeakingModel::qwen3_tts(),
        variant: SPEAKING_VARIANT.to_owned(),
        from: ActiveFrom::Pin,
        at: None,
    })
}

/// Deliberately narrower than a model spec, for the same reason
/// [`crate::dictate::DictateSettings`] is: a hand-edited URL, byte length or
/// digest is how a file arrives that nothing can verify.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TtsSettings {
    /// Off unless asked for. Turning it on offers the speaking model's
    /// install; the engine loads it on the first listener.
    #[serde(default)]
    pub enabled: bool,
    /// Where the model files and the public corpus live. Absent uses the
    /// platform cache directory.
    #[serde(default)]
    pub models_dir: Option<String>,
    /// The speaking model the role runs, by the catalogue's own variant
    /// name. Absent runs the built-in one.
    #[serde(default)]
    pub speaking_model: Option<String>,
    /// The voice the role speaks in: a speaker of the shipped public
    /// corpus. Absent leaves the model's default voice.
    #[serde(default)]
    pub voice: Option<String>,
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
            speaking_model: None,
            voice: None,
            language: None,
            max_utterance_seconds: default_max_utterance_seconds(),
        }
    }
}

impl TtsSettings {
    /// Where the model files land.
    pub fn models_dir(&self) -> Option<PathBuf> {
        match self.models_dir.as_deref() {
            Some(dir) => Some(crate::config::expand_home(dir)),
            None => dirs::cache_dir().map(|dir| dir.join("forge-tts")),
        }
    }
}

/// One voice the public corpus carries: a speaker, and the clip the role
/// reads in it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoiceChoice {
    pub speaker: String,
    /// The clip's length in whole seconds.
    pub seconds: u64,
}

/// The speaking role, as the settings page draws it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SpeakingView {
    pub variant: String,
    /// The weights' file name; the projector rides with it.
    pub file: String,
    pub state: DictateModelState,
    pub from: ActiveFrom,
    pub at: Option<String>,
}

/// The voice read: whether `[tts]` is on, where its files land, the model
/// the role runs, and the voices it can speak in.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TtsView {
    pub enabled: bool,
    pub models_dir: Option<PathBuf>,
    pub speaking: Option<SpeakingView>,
    /// Why the speaking model could not be resolved, when it could not.
    pub error: Option<String>,
    pub voice: Option<String>,
    /// Why the configured voice cannot be used, when it cannot.
    pub voice_error: Option<String>,
    pub voices: Vec<VoiceChoice>,
}

impl crate::Workspace {
    /// The models page's and the settings page's read.
    pub fn voices(&self) -> TtsView {
        let config = self.config_arc();
        let settings = &config.tts;
        // The running system's flag, not a saved value the restart has not
        // taken: the tool gate and the registered set read the same one.
        let enabled = self.voice_enabled();
        let (speaking, error) = if enabled {
            match self.resolved_speaking() {
                Ok(resolved) => {
                    let state = if self.speaking_files_present(&resolved) {
                        DictateModelState::Ready
                    } else {
                        DictateModelState::Pending
                    };
                    (
                        Some(SpeakingView {
                            variant: resolved.variant,
                            file: resolved.model.base.file.clone(),
                            state,
                            from: resolved.from,
                            at: resolved.at,
                        }),
                        None,
                    )
                }
                Err(error) => (None, Some(error)),
            }
        } else {
            (None, None)
        };
        let (voices, voice_error) = if enabled {
            let voices = self.installed_voices();
            let voice_error = settings
                .voice
                .as_deref()
                .and_then(|voice| crate::tts::unknown_voice(voice, &voices));
            (voices, voice_error)
        } else {
            (Vec::new(), None)
        };
        TtsView {
            enabled,
            models_dir: settings.models_dir(),
            speaking,
            error,
            voice: settings.voice.clone(),
            voice_error,
            voices,
        }
    }

    /// The speaking role's model, resolved from the config key, the
    /// runtime pick, or the compiled pin.
    pub(crate) fn resolved_speaking(&self) -> Result<ResolvedSpeaking, String> {
        let config = self.config_arc();
        let configured = config.tts.speaking_model.as_deref();
        let installed = self.installed_models();
        let pick =
            if configured.is_none() { self.active_choice(DictateRole::Speaking) } else { None };
        resolve_speaking(configured, &installed, pick)
    }

    /// The resolved role's two files, under the `[tts]` models directory.
    pub(crate) fn speaking_files(
        &self,
        resolved: &ResolvedSpeaking,
    ) -> Result<forge_tts::ModelFiles, String> {
        let dir = self
            .config_arc()
            .tts
            .models_dir()
            .ok_or_else(|| "no [tts] models directory resolves".to_owned())?;
        Ok(resolved.model.files(&dir))
    }

    /// Whether both of the resolved role's files are on disk.
    pub(crate) fn speaking_files_present(&self, resolved: &ResolvedSpeaking) -> bool {
        self.speaking_files(resolved).is_ok_and(|files| {
            files.model.is_file() && files.projector.as_ref().is_some_and(|file| file.is_file())
        })
    }

    /// The voices the installed public corpus carries, one row per speaker.
    /// Empty until the corpus is installed, and empty when this build's
    /// slice is not there.
    pub(crate) fn installed_voices(&self) -> Vec<VoiceChoice> {
        let Some(dir) = self.voice_corpus_dir() else {
            return Vec::new();
        };
        match forge_dictate::bench::public_voice_list(&dir) {
            Ok(rows) => rows
                .into_iter()
                .map(|row| VoiceChoice { speaker: row.speaker, seconds: row.seconds })
                .collect(),
            Err(error) => {
                tracing::warn!(
                    event_name = "tts_voice_list_unreadable",
                    path = %dir.display(),
                    %error,
                    "the public corpus could not be read; the voice list is empty"
                );
                Vec::new()
            }
        }
    }

    /// Where the shipped public corpus lives beside the speaking models,
    /// and where its clips are read from.
    pub(crate) fn voice_corpus_dir(&self) -> Option<PathBuf> {
        self.config_arc().tts.models_dir().map(|dir| dir.join("public-corpus"))
    }

    /// Install the shipped public corpus beside the speaking models, so the
    /// voice picker has its clips before the first read. One small write per
    /// boot, idempotent; a no-op unless `[tts]` is on.
    pub fn start_voice_corpus(self: &std::sync::Arc<Self>) {
        if !self.config_arc().tts.enabled {
            return;
        }
        let Some(dir) = self.config_arc().tts.models_dir() else {
            tracing::warn!(
                event_name = "tts_corpus_dir_unresolved",
                "no [tts] models directory resolves; the voice picker has no clips"
            );
            return;
        };
        tokio::spawn(async move {
            let installed = tokio::task::spawn_blocking(move || {
                forge_dictate::bench::install_public_corpus(&dir)
            })
            .await;
            match installed {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(
                    event_name = "tts_corpus_install_failed",
                    %error,
                    "the public corpus was not installed; the voice picker lists no voices"
                ),
                Err(join) => tracing::warn!(
                    event_name = "tts_corpus_install_failed",
                    %join,
                    "the public corpus was not installed; the voice picker lists no voices"
                ),
            }
        });
    }

    /// The clip one speaker's voice is read from, decoded at its own rate.
    pub(crate) fn voice_audio(&self, speaker: &str) -> Result<forge_tts::VoiceReference, String> {
        let dir = self.voice_corpus_dir().ok_or("no [tts] models directory resolves")?;
        let (pcm, sample_rate) = forge_dictate::bench::public_voice_audio(&dir, speaker)
            .map_err(|error| error.to_string())?;
        Ok(forge_tts::VoiceReference { pcm, sample_rate })
    }
}

/// The refusal an unknown `[tts] voice` answers with, or `None` when the
/// corpus carries it.
pub(crate) fn unknown_voice(voice: &str, voices: &[VoiceChoice]) -> Option<String> {
    if voices.iter().any(|row| row.speaker == voice) {
        return None;
    }
    Some(format!(
        "[tts] voice names {voice}, which neither the installed public corpus nor its manifest \
         carries"
    ))
}

#[cfg(test)]
mod tests {
    use super::{SPEAKING_VARIANT, TtsSettings, resolve_speaking, unknown_voice};
    use crate::install::{ActiveChoice, ActiveFrom};

    #[test]
    fn an_absent_tts_section_is_off() {
        let settings: TtsSettings = toml::from_str("").expect("parses");
        assert!(!settings.enabled);
        assert_eq!(settings.max_utterance_seconds, 120);
        assert_eq!(settings.speaking_model, None);
        assert_eq!(settings.voice, None);
    }

    #[test]
    fn an_unknown_tts_key_fails_the_load() {
        let error = toml::from_str::<TtsSettings>("speek = \"x\"").expect_err("refused");
        assert!(error.to_string().contains("speek"));
    }

    /// The built-in speaking variant resolves through the config key, and
    /// the model it names is the compiled one.
    #[test]
    fn the_builtin_speaking_variant_resolves() {
        let resolved =
            resolve_speaking(Some(SPEAKING_VARIANT), &[], None).expect("the variant resolves");

        assert_eq!(resolved.variant, SPEAKING_VARIANT);
        assert_eq!(resolved.model.base.file, "Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf");
        assert_eq!(resolved.model.projector.file, "mmproj-Qwen3-TTS-12Hz-1.7B-Base-Q8_0.gguf");
        assert_eq!(
            resolved.from,
            ActiveFrom::Config {
                key: "speaking_model".to_owned(),
                variant: SPEAKING_VARIANT.to_owned(),
            },
            "the page can name the key that holds the role"
        );
    }

    /// A key naming a variant nothing carries is refused by name, rather
    /// than silently running the pin.
    #[test]
    fn an_unknown_speaking_model_is_refused_by_name() {
        let error = resolve_speaking(Some("no-such-voice"), &[], None).expect_err("refused");

        assert!(error.contains("[tts] speaking_model"), "got: {error}");
        assert!(error.contains("no-such-voice"), "got: {error}");
    }

    /// With no key and no pick, the compiled pin stands.
    #[test]
    fn the_pin_resolves_the_builtin_model() {
        let resolved = resolve_speaking(None, &[], None).expect("the pin resolves");

        assert_eq!(resolved.variant, SPEAKING_VARIANT);
        assert_eq!(resolved.from, ActiveFrom::Pin);
        assert_eq!(resolved.at, None);
    }

    /// A pick whose variant no record carries falls to the pin: forge's own
    /// bookkeeping must not refuse the boot.
    #[test]
    fn a_pick_without_a_record_resolves_to_the_pin() {
        let pick = ActiveChoice {
            variant: SPEAKING_VARIANT.to_owned(),
            at: "2026-11-02T00:00:00Z".to_owned(),
        };

        let resolved =
            resolve_speaking(None, &[], Some(pick)).expect("a stranded pick still boots");

        assert_eq!(resolved.from, ActiveFrom::Pin);
    }

    /// An unknown `[tts] voice` is refused by name, and a known one is not.
    #[test]
    fn an_unknown_voice_is_refused_by_name() {
        let voices = vec![super::VoiceChoice { speaker: "6930".to_owned(), seconds: 4 }];

        let error = unknown_voice("nobody", &voices).expect("the unknown id is refused");
        assert!(error.contains("[tts] voice"), "got: {error}");
        assert!(error.contains("nobody"), "got: {error}");
        assert_eq!(unknown_voice("6930", &voices), None, "an installed speaker is accepted");
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
