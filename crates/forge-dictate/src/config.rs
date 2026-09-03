//! Configuration for a dictation engine.

use std::path::PathBuf;
use std::time::Duration;

use crate::normalize::NormalizeOptions;

/// One model file: where it comes from and what it must hash to.
///
/// The size and digest are what let [`crate::prepare`] reject a
/// truncated or corrupt file before a runtime ever opens it.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// File name under the models directory.
    pub file: String,
    /// Source the file is downloaded from when it is absent.
    pub url: String,
    /// Exact byte length of the complete file.
    pub size: u64,
    /// Lowercase hex SHA-256 of the complete file.
    pub sha256: String,
}

impl ModelSpec {
    /// Cohere Transcribe, Q4_K_M quantisation. The default ASR model.
    pub fn cohere_transcribe_q4_k_m() -> Self {
        Self {
            file: "cohere-transcribe-03-2026-Q4_K_M.gguf".into(),
            url: "https://huggingface.co/handy-computer/cohere-transcribe-03-2026-gguf/resolve/main/cohere-transcribe-03-2026-Q4_K_M.gguf".into(),
            size: 1_558_162_944,
            sha256: "0ea56826d8bd5d74b7143a4a04e022dc1bb75452cfae49d98b6acb0c1d16a1fb".into(),
        }
    }

    /// S1-mini, f16. The default normalizer.
    pub fn s1_mini_f16() -> Self {
        Self {
            file: "s1-mini-f16.gguf".into(),
            url: "https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/main/s1-mini-f16.gguf"
                .into(),
            size: 1_509_347_232,
            sha256: "0370da4f1bae19e3150bcafa33c5d396c15f97bf25519540a3e013db5cc00af4".into(),
        }
    }
}

/// Configuration for one dictation engine.
///
/// Construct via [`ConfigBuilder`] rather than populating directly.
/// Values are validated where they are used, never here: an unreadable
/// models directory surfaces from [`crate::prepare`], not from
/// [`ConfigBuilder::build`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory holding model files. None resolves to a subdirectory of
    /// the platform cache directory.
    pub models_dir: Option<PathBuf>,
    /// Speech recognition model.
    pub asr_model: ModelSpec,
    /// Model that rewrites raw recognition output into clean text. None
    /// leaves the recognition output as-is and fetches nothing for it.
    ///
    /// When set, the engine loads it beside the recognition model and
    /// rewrites every transcript: [`crate::Transcript::text`] carries the
    /// result and `asr` the recognised text it came from. When `None`
    /// the two are equal, which is a supported configuration rather than
    /// a degraded one.
    pub normalizer: Option<ModelSpec>,
    /// How the normalizer rewrites text, when one is configured.
    ///
    /// `styling` is the axis a person picks per recording; `k` and
    /// `ngram` are decoder tuning and do not belong in a user-facing
    /// picker beside it. Overridable per call via
    /// [`crate::Engine::transcribe_with`], so this is the default rather
    /// than a fixed choice.
    pub normalize_options: NormalizeOptions,
    /// Input to record from, by [`crate::Device::id`]. None means the
    /// system default, explicitly: a host that does not care never has
    /// to enumerate. Keyed on the id rather than the name because the id
    /// is what survives a restart and a rename.
    pub device: Option<String>,
    /// Spoken language hint. None autodetects.
    pub language: Option<String>,
    /// Upper bound on a single capture. A capture nobody stops ends
    /// itself here rather than holding the input device indefinitely.
    ///
    /// Costs memory eagerly: the whole cap is reserved when recording
    /// starts, at 4 bytes a sample, so the 30 min default reserves about
    /// 110 MiB and an hour reserves 219 MiB, whatever the utterance turns
    /// out to be. Reserved rather than grown because the audio callback
    /// must not allocate. Clamped to an hour.
    pub max_capture: Duration,
    /// Peak input level, in dBFS, below which a capture counts as
    /// silence rather than as an empty transcript.
    pub silence_floor: f32,
    /// Directory the crate writes per-take diagnostics into: the
    /// capture audio and every transcription stage, best-effort, under
    /// `crate::diagnostics`. `None` writes nothing - diagnostics are
    /// the host's choice of directory, never a requirement.
    pub diagnostics_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            models_dir: None,
            asr_model: ModelSpec::cohere_transcribe_q4_k_m(),
            normalizer: Some(ModelSpec::s1_mini_f16()),
            normalize_options: NormalizeOptions::default(),
            device: None,
            language: None,
            max_capture: Duration::from_secs(30 * 60),
            silence_floor: -50.0,
            diagnostics_dir: None,
        }
    }
}

/// Builder for [`Config`].
#[derive(Debug, Clone, Default)]
pub struct ConfigBuilder {
    inner: Config,
}

impl ConfigBuilder {
    /// Start from defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the directory model files live in.
    pub fn models_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.inner.models_dir = Some(dir.into());
        self
    }

    /// Override the speech recognition model.
    pub fn asr_model(mut self, spec: ModelSpec) -> Self {
        self.inner.asr_model = spec;
        self
    }

    /// Set or clear the normalizer. Passing `None` skips normalization
    /// and its download.
    pub fn normalizer(mut self, spec: impl Into<Option<ModelSpec>>) -> Self {
        self.inner.normalizer = spec.into();
        self
    }

    /// Record from a specific input rather than the system default,
    /// identified by [`crate::Device::id`].
    pub fn device(mut self, id: impl Into<String>) -> Self {
        self.inner.device = Some(id.into());
        self
    }

    /// Set the default normalizer options. Per-call overrides go
    /// through [`crate::Engine::transcribe_with`].
    pub fn normalize_options(mut self, options: NormalizeOptions) -> Self {
        self.inner.normalize_options = options;
        self
    }

    /// Set the spoken language hint.
    pub fn language(mut self, language: impl Into<String>) -> Self {
        self.inner.language = Some(language.into());
        self
    }

    /// Cap the length of a single capture.
    pub fn max_capture(mut self, max: Duration) -> Self {
        self.inner.max_capture = max;
        self
    }

    /// Set the silence threshold in dBFS.
    pub fn silence_floor(mut self, dbfs: f32) -> Self {
        self.inner.silence_floor = dbfs;
        self
    }

    /// Point per-take diagnostics at a directory. `None` (the default)
    /// writes nothing.
    pub fn diagnostics_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.inner.diagnostics_dir = Some(dir.into());
        self
    }

    /// Finalise and return the [`Config`].
    pub fn build(self) -> Config {
        self.inner
    }
}

#[cfg(test)]
mod tests_config {
    use super::*;
    use crate::normalize::Styling;
    use std::path::Path;

    #[test]
    fn defaults_match_the_documented_values() {
        let cfg = ConfigBuilder::new().build();

        assert!(cfg.models_dir.is_none(), "models_dir defaults to the platform cache directory");
        assert_eq!(
            cfg.asr_model.file, "cohere-transcribe-03-2026-Q4_K_M.gguf",
            "the default asr model is Cohere Transcribe at Q4_K_M"
        );
        assert!(cfg.normalizer.is_some(), "normalization is on by default");
        assert_eq!(
            cfg.normalize_options,
            NormalizeOptions::default(),
            "the shipped normalizer settings are the default ones; nothing else pins this"
        );
        assert!(cfg.language.is_none(), "language defaults to autodetect");
        assert_eq!(
            cfg.max_capture,
            Duration::from_secs(30 * 60),
            "max_capture defaults to 30 minutes"
        );
        assert!(
            (cfg.silence_floor - -50.0).abs() < f32::EPSILON,
            "silence_floor defaults to -50 dBFS, got {}",
            cfg.silence_floor
        );
    }

    #[test]
    fn setters_round_trip_through_build() {
        let cfg = ConfigBuilder::new()
            .models_dir("/models")
            .asr_model(ModelSpec::s1_mini_f16())
            .language("en")
            .max_capture(Duration::from_secs(5))
            .silence_floor(-30.0)
            .diagnostics_dir("/diag")
            .normalizer(ModelSpec::cohere_transcribe_q4_k_m())
            .normalize_options(NormalizeOptions {
                styling: Styling::Casual,
                ..NormalizeOptions::default()
            })
            .build();

        assert_eq!(
            cfg.models_dir.as_deref(),
            Some(Path::new("/models")),
            "an explicit models_dir must override the platform cache default"
        );
        assert_eq!(
            cfg.asr_model.file,
            ModelSpec::s1_mini_f16().file,
            "asr_model must replace the default spec outright"
        );
        assert_eq!(
            cfg.language.as_deref(),
            Some("en"),
            "a language hint must reach the field that autodetects when unset"
        );
        assert_eq!(
            cfg.max_capture,
            Duration::from_secs(5),
            "max_capture must carry the caller's cap, not the default"
        );
        assert!(
            (cfg.silence_floor - -30.0).abs() < f32::EPSILON,
            "silence_floor must carry the caller's threshold, not the -50 dBFS default, got {}",
            cfg.silence_floor
        );
        assert_eq!(
            cfg.diagnostics_dir.as_deref(),
            Some(Path::new("/diag")),
            "diagnostics_dir must carry the caller's store location, not be dropped by the builder"
        );
        assert_eq!(
            cfg.normalizer.map(|n| n.file),
            Some(ModelSpec::cohere_transcribe_q4_k_m().file),
            "a bare ModelSpec must reach the Option field"
        );

        assert_eq!(
            cfg.normalize_options.styling,
            Styling::Casual,
            "normalize_options must reach the field the engine reads, not be dropped by the builder"
        );

        let off = ConfigBuilder::new().normalizer(None).build();
        assert!(off.normalizer.is_none(), "the same setter must also clear the normalizer");
    }
}
