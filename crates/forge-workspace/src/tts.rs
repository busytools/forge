//! The `[tts]` section: what the speaking role runs, the Hugging Face
//! install that supplies it, and the read the models page and the settings
//! card draw.

use std::path::PathBuf;

use forge_dictate::{ModelFacts, ModelSpec};
use serde::Deserialize;

use crate::dictate::{DictateFailure, DictateModel, DictateModelState, DictateRole};
use crate::install::InstallState;

/// The speaking model's own name. `[tts] speaking_model` accepts this, or one
/// of [`VARIANTS`]' names to pin a quantisation.
pub const SPEAKING_MODEL: &str = "hexgrad/Kokoro-82M";

/// The Hub repository publishing the ONNX export and the style files the
/// engine loads.
pub const HUB_REPO: &str = "onnx-community/Kokoro-82M-v1.0-ONNX";

/// The variant the role runs when nothing chooses another: the int8 export.
pub const PINNED_VARIANT: &str = "kokoro-82m-int8";

/// One ONNX variant of the model the Hub publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeakingVariant {
    /// The name `[tts] speaking_model` accepts.
    pub variant: &'static str,
    /// The file's path in the repository.
    pub path: &'static str,
    /// The file's own name under the models directory.
    pub file: &'static str,
    pub quant: &'static str,
    /// The release's byte length, for the row drawn before an install.
    pub size: u64,
    /// The release's LFS digest, for the row drawn before an install.
    pub sha256: &'static str,
}

/// The variants the speaking role can run, the int8 model the benchmark
/// measured among them.
pub const VARIANTS: &[SpeakingVariant] = &[
    SpeakingVariant {
        variant: "kokoro-82m-fp32",
        path: "onnx/model.onnx",
        file: "model.onnx",
        quant: "fp32",
        size: 325_532_232,
        sha256: "8fbea51ea711f2af382e88c833d9e288c6dc82ce5e98421ea61c058ce21a34cb",
    },
    SpeakingVariant {
        variant: "kokoro-82m-fp16",
        path: "onnx/model_fp16.onnx",
        file: "model_fp16.onnx",
        quant: "fp16",
        size: 163_234_740,
        sha256: "ba4527a874b42b21e35f468c10d326fdff3c7fc8cac1f85e9eb6c0dfc35c334a",
    },
    SpeakingVariant {
        variant: "kokoro-82m-int8",
        path: "onnx/model_quantized.onnx",
        file: "model_quantized.onnx",
        quant: "int8",
        size: 92_361_116,
        sha256: "fbae9257e1e05ffc727e951ef9b9c98418e6d79f1c9b6b13bd59f5c9028a1478",
    },
];

impl SpeakingVariant {
    /// The variant a name selects: the model's own name picks the pin, and a
    /// variant name picks itself.
    pub fn named(name: &str) -> Option<Self> {
        if name == SPEAKING_MODEL {
            return Some(Self::pinned());
        }
        VARIANTS.iter().copied().find(|variant| variant.variant == name)
    }

    /// The compiled default.
    pub fn pinned() -> Self {
        VARIANTS
            .iter()
            .copied()
            .find(|variant| variant.variant == PINNED_VARIANT)
            .unwrap_or(VARIANTS[0])
    }

    /// The row a spec-driven read draws before an install lands.
    pub fn spec(&self) -> ModelSpec {
        ModelSpec {
            file: self.file.to_owned(),
            url: format!("https://huggingface.co/{HUB_REPO}/resolve/main/{}", self.path),
            size: self.size,
            sha256: Some(self.sha256.to_owned()),
            facts: ModelFacts {
                quant: Some(self.quant.to_owned()),
                params: Some(82_000_000),
                license: Some("Apache-2.0".to_owned()),
                runtime: Some("onnx".to_owned()),
            },
        }
    }
}

/// One style file the install fetched.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoiceFile {
    /// The voice id: the file's stem.
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// What one installed variant is made of, as the install recorded it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpeakingInstall {
    pub variant: String,
    /// The Hub revision every file was fetched at, so the set is pinned.
    pub revision: String,
    /// The model's file name under the models directory.
    pub model: String,
    pub model_size: u64,
    pub model_sha256: String,
    /// The style files, by voice id.
    pub voices: Vec<VoiceFile>,
    /// RFC 3339.
    pub at: String,
}

impl SpeakingInstall {
    /// The model's spec, off the record rather than off the release.
    pub fn spec(&self) -> ModelSpec {
        let variant = SpeakingVariant::named(&self.variant).unwrap_or_else(SpeakingVariant::pinned);
        ModelSpec {
            file: self.model.clone(),
            url: format!(
                "https://huggingface.co/{HUB_REPO}/resolve/{}/{}",
                self.revision, variant.path
            ),
            size: self.model_size,
            sha256: Some(self.model_sha256.clone()),
            facts: variant.spec().facts,
        }
    }

    /// The voice ids this install carried.
    pub fn voice_names(&self) -> Vec<String> {
        self.voices.iter().map(|voice| voice.name.clone()).collect()
    }
}

/// Deliberately narrower than a model spec, for the same reason
/// [`crate::dictate::DictateSettings`] is: a hand-edited URL, byte length or
/// digest is how a file arrives that nothing can verify.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TtsSettings {
    /// Off unless asked for. Turning it on makes the speaking role available;
    /// the model installs from the models page or the settings card.
    #[serde(default)]
    pub enabled: bool,
    /// Where the model files live. Absent uses the platform cache directory.
    #[serde(default)]
    pub models_dir: Option<String>,
    /// The model the role runs. Absent uses the compiled pin; an unknown
    /// name is refused by name.
    #[serde(default)]
    pub speaking_model: Option<String>,
    /// The voice every utterance renders in. Absent uses the engine's own
    /// default; an id that is not installed is refused by name.
    #[serde(default)]
    pub voice: Option<String>,
    /// Ghost of the retired `[tts] language` hint: read only so a stale
    /// config keeps loading. The voice id is the language choice (the 54
    /// voices cover eight languages), so there is nothing for the key to set.
    #[serde(default)]
    pub language: Option<toml::Value>,
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
    /// The variant a configured name selects, refusing an unknown name the
    /// way `[dictate] transcribe_model` does.
    pub fn configured_variant(&self) -> Result<Option<SpeakingVariant>, String> {
        let Some(name) = self.speaking_model.as_deref() else {
            return Ok(None);
        };
        SpeakingVariant::named(name)
            .map(Some)
            .ok_or_else(|| format!("[tts] speaking_model names {name}, which is not a known model"))
    }

    /// Where the model files land.
    pub fn models_dir(&self) -> Option<PathBuf> {
        match self.models_dir.as_deref() {
            Some(dir) => Some(crate::config::expand_home(dir)),
            None => dirs::cache_dir().map(|dir| dir.join("forge-tts")),
        }
    }

    /// The directory the style files live in.
    pub fn voices_dir(&self) -> Option<PathBuf> {
        self.models_dir().map(|dir| dir.join("voices"))
    }

    /// The model a role runs before anything is installed: the configured
    /// variant, else the pin.
    pub fn pin(&self) -> ModelSpec {
        self.configured_variant().ok().flatten().unwrap_or_else(SpeakingVariant::pinned).spec()
    }
}

/// The speaking role's model rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TtsSnapshot {
    pub models: Vec<DictateModel>,
}

/// One variant the page draws: whether it is installed, and whether it runs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpeakingVariantView {
    pub variant: String,
    pub file: String,
    pub quant: String,
    pub size: u64,
    pub installed: bool,
    pub active: bool,
}

/// Where the active speaking model came from. A bare name on the wire: the
/// speaking read has one `from`, and a tag object would nest it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakingFrom {
    /// `[tts] speaking_model` names it.
    Config,
    /// The last pick made on this page.
    Installed,
    /// The compiled default.
    Pin,
}

/// The voice read: whether `[tts]` is on, where the files land, the speaking
/// model's state, and the voices that model installed.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TtsView {
    pub enabled: bool,
    pub snapshot: TtsSnapshot,
    pub models_dir: Option<PathBuf>,
    /// The model's own name, which `[tts] speaking_model` accepts.
    pub speaking_model: String,
    /// The variant the role runs.
    pub active: SpeakingVariantView,
    /// Where the active variant's choice came from.
    pub from: SpeakingFrom,
    /// Every variant the role can run.
    pub variants: Vec<SpeakingVariantView>,
    /// The installed voice ids, sorted.
    pub voices: Vec<String>,
    /// The configured voice; absent means the engine's default.
    pub voice: Option<String>,
    /// The last preview's render failure, when one failed after the command
    /// was accepted. Cleared by the next preview.
    pub preview_error: Option<String>,
}

impl crate::Workspace {
    /// Every speaking install this machine has recorded.
    pub fn speaking_installs(&self) -> Vec<SpeakingInstall> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Vec::new() };
        match crate::store::speaking_models::installed(db) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    event_name = "speaking_installed_read_failed",
                    %error,
                    "the speaking installs could not be read; the page shows none"
                );
                Vec::new()
            }
        }
    }

    /// One installed variant, when this machine has it.
    pub fn speaking_install(&self, variant: &str) -> Option<SpeakingInstall> {
        self.speaking_installs().into_iter().find(|install| install.variant == variant)
    }

    /// The variant the role runs: the configured name, else the runtime
    /// pick, else the compiled pin.
    pub fn speaking_variant(&self) -> (SpeakingVariant, SpeakingFrom) {
        if let Ok(Some(variant)) = self.config_arc().tts.configured_variant() {
            return (variant, SpeakingFrom::Config);
        }
        if let Some(pick) = self.active_choice(DictateRole::Speaking)
            && let Some(variant) = SpeakingVariant::named(&pick.variant)
        {
            return (variant, SpeakingFrom::Installed);
        }
        (SpeakingVariant::pinned(), SpeakingFrom::Pin)
    }

    /// The files the active variant loads, when this machine has them.
    pub fn speaking_files(&self) -> Option<forge_tts::ModelFiles> {
        let (variant, _) = self.speaking_variant();
        let install = self.speaking_install(variant.variant)?;
        self.files_for_install(&install)
    }

    /// The files one installed variant is made of.
    pub fn files_for_install(&self, install: &SpeakingInstall) -> Option<forge_tts::ModelFiles> {
        let dir = self.config_arc().tts.models_dir()?;
        let model = dir.join(&install.model);
        let voices = dir.join("voices");
        let digests =
            std::iter::once((model.clone(), install.model_sha256.clone()))
                .chain(install.voices.iter().map(|voice| {
                    (voices.join(format!("{}.bin", voice.name)), voice.sha256.clone())
                }))
                .collect();
        Some(forge_tts::ModelFiles::new(model, voices).with_digests(digests))
    }

    /// The models page's and the settings card's read.
    pub fn voices(&self) -> TtsView {
        let config = self.config_arc();
        let settings = &config.tts;
        let (active, from) = self.speaking_variant();
        let installs = self.speaking_installs();
        let installed = |variant: &str| installs.iter().any(|row| row.variant == variant);
        let row = |variant: &SpeakingVariant| SpeakingVariantView {
            variant: variant.variant.to_owned(),
            file: variant.file.to_owned(),
            quant: variant.quant.to_owned(),
            size: variant.size,
            installed: installed(variant.variant),
            active: variant.variant == active.variant,
        };
        // The row is installed only when the record is present and the model
        // file is still on disk: an evicted file reads as missing, exactly as
        // the boot trigger reads it.
        let active_installed =
            installed(active.variant) && self.speaking_files().is_some_and(|f| f.model.exists());
        let active_voices = installs
            .iter()
            .find(|row| row.variant == active.variant)
            .map(SpeakingInstall::voice_names)
            .unwrap_or_default();
        // While the speaking install runs, the row says what it is doing -
        // fetching or failed - rather than "waiting". The models page's IN USE
        // row and the settings speaking card draw it; the page's install and
        // the boot's are the same state, so one mapping covers both.
        let state = if active_installed {
            DictateModelState::Ready
        } else {
            match &*self.dictate_install.lock() {
                InstallState::Downloading { file, got, total } if file.as_str() == active.file => {
                    DictateModelState::Downloading {
                        downloaded: *got,
                        total: *total,
                        resumed_from: None,
                    }
                }
                InstallState::Failed { file, reason } if file.as_str() == active.file => {
                    DictateModelState::Failed(DictateFailure::Other { message: reason.clone() })
                }
                _ => DictateModelState::Pending,
            }
        };
        // The running system's flag, not a saved value the restart has not
        // taken: the tool gate and the registered set read the same one.
        let enabled = self.voice_enabled();
        TtsView {
            enabled,
            snapshot: TtsSnapshot {
                models: if enabled {
                    vec![DictateModel {
                        role: DictateRole::Speaking,
                        file: active.file.to_owned(),
                        state,
                    }]
                } else {
                    Vec::new()
                },
            },
            models_dir: settings.models_dir(),
            speaking_model: SPEAKING_MODEL.to_owned(),
            active: row(&active),
            from,
            variants: VARIANTS.iter().map(row).collect(),
            voices: active_voices,
            voice: settings.voice.clone(),
            preview_error: self.voice_preview_error.lock().ok().and_then(|slot| slot.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PINNED_VARIANT, SPEAKING_MODEL, SpeakingVariant, TtsSettings};

    #[test]
    fn an_absent_tts_section_is_off() {
        let settings: TtsSettings = toml::from_str("").expect("parses");
        assert!(!settings.enabled);
        assert_eq!(settings.max_utterance_seconds, 120);
        assert!(settings.voice.is_none());
    }

    #[test]
    fn an_unknown_tts_key_fails_the_load() {
        let error = toml::from_str::<TtsSettings>("speek = \"x\"").expect_err("refused");
        assert!(error.to_string().contains("speek"));
    }

    #[test]
    fn the_models_own_name_selects_the_pinned_variant() {
        let variant = SpeakingVariant::named(SPEAKING_MODEL).expect("the name is known");
        assert_eq!(variant.variant, PINNED_VARIANT);
    }

    #[test]
    fn a_variant_name_selects_itself() {
        let variant = SpeakingVariant::named("kokoro-82m-fp16").expect("the name is known");
        assert_eq!(variant.file, "model_fp16.onnx");
    }

    /// The speaking read's `from` is a bare name: the client compares it
    /// against a list of strings, and a tag object would read as unknown on
    /// both pages.
    #[test]
    fn the_speaking_source_crosses_as_a_bare_name() {
        let value = serde_json::to_value(super::SpeakingFrom::Installed).expect("it serializes");
        assert_eq!(value, serde_json::json!("installed"));
    }

    #[test]
    fn a_configured_name_that_is_not_a_model_is_refused_by_name() {
        let settings: TtsSettings =
            toml::from_str("speaking_model = \"hexgrad/Kokoro-90M\"").expect("parses");
        let error = settings.configured_variant().expect_err("refused");
        assert!(error.contains("hexgrad/Kokoro-90M"), "got: {error}");
        assert!(error.contains("speaking_model"), "got: {error}");
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

    /// The speaking row follows the install in flight: a boot's download is
    /// drawn as `Downloading`, not `Pending`, so the models page's IN USE row
    /// and the settings speaking card read "fetching" while the model lands.
    /// Deleting the mapping leaves the row `Pending` and this test red.
    #[tokio::test]
    async fn the_speaking_row_carries_the_downloading_state_while_the_install_runs() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.tts.enabled = true;
        let (workspace, _updates) =
            crate::Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
                .expect("the stub builds");
        let variant = SpeakingVariant::pinned();
        *workspace.dictate_install.lock() = crate::install::InstallState::Downloading {
            file: variant.file.to_owned(),
            got: 12,
            total: 100,
        };
        let view = workspace.voices();
        let row = view.snapshot.models.first().expect("the speaking row is drawn");
        assert_eq!(
            row.state,
            crate::dictate::DictateModelState::Downloading {
                downloaded: 12,
                total: 100,
                resumed_from: None,
            }
        );
    }

    /// The speaking row carries the failed state too: an offline boot's
    /// install lands on `InstallState::Failed`, and the row must read
    /// "failed" rather than "waiting". Making the `Failed` arm unreachable
    /// leaves this red.
    #[tokio::test]
    async fn the_speaking_row_carries_the_failed_state_when_the_install_fails() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.tts.enabled = true;
        let (workspace, _updates) =
            crate::Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
                .expect("the stub builds");
        let variant = SpeakingVariant::pinned();
        *workspace.dictate_install.lock() = crate::install::InstallState::Failed {
            file: variant.file.to_owned(),
            reason: "the Hub is down".to_owned(),
        };
        let view = workspace.voices();
        let row = view.snapshot.models.first().expect("the speaking row is drawn");
        assert!(
            matches!(row.state, crate::dictate::DictateModelState::Failed(_)),
            "the row drew the failed install as {:?}",
            row.state
        );
        let in_use = workspace.dictate_models().in_use;
        assert!(
            in_use.iter().any(|model| {
                model.role == crate::dictate::DictateRole::Speaking
                    && matches!(model.state, crate::dictate::DictateModelState::Failed(_))
            }),
            "the in-use copy carried the failed install"
        );
    }

    /// A download for another variant is not drawn on the active row: the
    /// file-match guard keeps the fp16 install's progress off the fp32 row.
    #[tokio::test]
    async fn the_speaking_row_ignores_another_variants_download() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.tts.enabled = true;
        let (workspace, _updates) =
            crate::Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
                .expect("the stub builds");
        let active = SpeakingVariant::pinned();
        let other = SpeakingVariant::named("kokoro-82m-fp16").expect("the fp16 variant is known");
        assert_ne!(other.file, active.file, "the test needs two different files");
        *workspace.dictate_install.lock() = crate::install::InstallState::Downloading {
            file: other.file.to_owned(),
            got: 12,
            total: 100,
        };
        let view = workspace.voices();
        let row = view.snapshot.models.first().expect("the speaking row is drawn");
        assert_eq!(
            row.state,
            crate::dictate::DictateModelState::Pending,
            "another variant's download was drawn on the active row"
        );
    }
}
